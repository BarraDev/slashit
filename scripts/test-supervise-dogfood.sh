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

EXPECTED_TOTAL=138

# Linux only: the supervisor and this harness read /proc and use flock and setsid.
[ "$(uname -s)" = Linux ] || { echo "FAIL $0 is Linux only (it exercises /proc, flock and setsid)" >&2; exit 1; }

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
sup="$root/scripts/supervise-dogfood.sh"
[ -f "$sup" ] || { echo "FAIL $sup is missing" >&2; exit 1; }

work=$(mktemp -d "${TMPDIR:-/tmp}/supervise-dogfood-test.XXXXXX")
trap 'cleanup_all' EXIT

passed=0
# Disposable processes this harness started, as "pid starttime". Each one is
# signaled only while its start time still matches (see signal_owned).
foreign_pids=()
failed=0

ok() {
  echo "ok   $*"
  passed=$((passed + 1))
}

bad() {
  echo "FAIL $*"
  failed=$((failed + 1))
}

# Process identity -------------------------------------------------------------
# A pid number is reused by the kernel, so every signal this harness sends goes
# through one of these checks: the target must still have the start time that
# was captured when this harness (or its fixture) created it.

proc_start() { # pid -> start time (field 22 of /proc/<pid>/stat), or fails
  local stat rest
  stat=$(cat "/proc/$1/stat" 2>/dev/null) || return 1
  rest=${stat##*) }
  set -- $rest
  printf '%s\n' "${20}"
}

# Records a disposable process this harness started, with its identity.
own() { # pid
  foreign_pids+=("$1 $(proc_start "$1")")
}

# Signals "pid starttime" only while the process still has that start time.
signal_owned() { # SIGNAL "pid starttime"
  local sig=$1 pid=${2%% *} start=${2#* } now
  now=$(proc_start "$pid" 2>/dev/null) || return 1
  [ -n "$start" ] && [ "$now" = "$start" ] || return 1
  kill -"$sig" "$pid" 2>/dev/null
}

# A process this run's fixture started carries the run's unique stub directory
# in its environment, and only such a process is ever a supervisor or instance
# target. A recycled pid, or an operator's own process, does not carry it.
owned_by_fixture() { # pid
  local envs
  envs=$({ tr '\0' '\n' <"/proc/$1/environ"; } 2>/dev/null) || return 1
  case $'\n'"$envs"$'\n' in
    *$'\n'"SLASHIT_TEST_STUB_DIR=$stub_dir"$'\n'*) return 0 ;;
  esac
  return 1
}

# Every live descendant of a pid, found from the parent pid in /proc.
descendants_of() { # pid
  local parent=$1 c stat rest ppid
  for c in /proc/[0-9]*; do
    c=${c#/proc/}
    stat=$(cat "/proc/$c/stat" 2>/dev/null) || continue
    rest=${stat##*) }
    set -- $rest
    ppid=$2
    [ "$ppid" = "$parent" ] || continue
    echo "$c"
    descendants_of "$c"
  done
}

# The supervisor a background `sv start` job became: a descendant of that job
# whose command line is exactly this checkout's supervisor run as `start`, and
# which carries this run's fixture environment. Prints "pid starttime".
fixture_supervisor() { # job
  local pid cmd
  for pid in $(descendants_of "$1"); do
    cmd=$({ tr '\0' ' ' <"/proc/$pid/cmdline"; } 2>/dev/null) || continue
    [ "$cmd" = "bash $sup start " ] || continue
    owned_by_fixture "$pid" || continue
    printf '%s %s\n' "$pid" "$(proc_start "$pid")"
    return 0
  done
  return 1
}

cleanup_all() {
  cleanup_instances
  local rec
  if [ -n "${fe_srv:-}" ]; then
    kill "$fe_srv" 2>/dev/null || true
    wait "$fe_srv" 2>/dev/null || true
    fe_srv=
  fi
  for rec in "${foreign_pids[@]+"${foreign_pids[@]}"}"; do
    signal_owned KILL "$rec" || true
  done
  rm -rf "$work"
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

# SLASHIT_TEST_STUB_DIR is deliberately NOT exported: it is the fixture marker
# that owned_by_fixture looks for, so only the supervisor (through BASEENV) and
# its stub children carry it, never the harness's own disposable processes.
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
  exec sleep 300
fi
exit 0
STUB
chmod +x "$bin_dir/slashit-ui"
# Backdated so the stub is unambiguously older than any instance it starts: the
# stale-instance check compares whole-second mtimes with process start, and a
# stub written in the instance's start second reads as rebuilt (a CI race).
touch -d '2000-01-01 00:00:00' "$bin_dir/slashit-ui"
# The stub is an embedded build for the supervisor's frontend gate, bound to its
# own sha256, so no case depends on a dev server on localhost:1420.
printf 'mode=embedded\nsha256=%s\nwasm=stub_bg.wasm\n' "$(sha256sum "$bin_dir/slashit-ui" | cut -d' ' -f1)" >"$bin_dir/slashit-ui.frontend"

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
echo "$last" >>"$stub_dir/calls"
case $last in
  --version)
    echo "slashit 9.9.9-stub"
    exit 0
    ;;
  ping)
    if [ -n "${SLASHIT_TEST_CLI_HANG:-}" ]; then
      exec sleep 300
    fi
    if [ -n "${SLASHIT_TEST_CLI_NEVER_ANSWERS:-}" ]; then
      exit 1
    fi
    if [ -n "${SLASHIT_TEST_CLI_FAKE_PING:-}" ]; then
      cat "$SLASHIT_TEST_CLI_FAKE_PING"
      exit 0
    fi
    # Answers only while the instance process is alive, as a real one would: a
    # forced kill leaves the pid file behind, and that is not an answer.
    if [ -f "$stub_dir/pid" ] && kill -0 "$(cat "$stub_dir/pid")" 2>/dev/null; then
      if [ -n "${SLASHIT_TEST_CLI_REPLACE_RECORD:-}" ]; then
        # Replaces the launch record after the provisional write, so the
        # verified write is the one that fails.
        rm -f "$SLASHIT_TEST_CLI_REPLACE_RECORD"
        mkdir "$SLASHIT_TEST_CLI_REPLACE_RECORD"
      fi
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
  SLASHIT_TEST_STUB_DIR="$stub_dir"
  SLASHIT_DOGFOOD_STATE="$state"
  SLASHIT_DOGFOOD_RUNTIME="$runtime"
  SLASHIT_DOGFOOD_BINARY="$bin_dir/slashit-ui"
  SLASHIT_DOGFOOD_CLI="$stub_dir/slashit"
  SLASHIT_DOGFOOD_DEV_URL=http://127.0.0.1:9/
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

# The declared environment, for disposable foreign processes that must look
# declared (only their mode or endpoint can then refuse them).
declared_env=(
  XDG_CONFIG_HOME="$state/config"
  XDG_DATA_HOME="$state/data"
  XDG_CACHE_HOME="$state/cache"
  XDG_STATE_HOME="$state"
  XDG_RUNTIME_DIR="$runtime"
  GH_CONFIG_DIR="$HOME/.config/gh"
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
  # The pid file can outlive its instance, and its pid can then be reused by an
  # unrelated process. Only a process that carries this run's fixture marker is
  # the stub instance and may be signaled.
  local pid
  pid=$(cat "$stub_dir/pid" 2>/dev/null) || pid=
  if [ -n "$pid" ] && owned_by_fixture "$pid"; then
    kill -9 "$pid" 2>/dev/null || true
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

echo "== platform guard"
# A shim `uname` that reports another platform, put first on PATH for the
# supervisor only: the guard must refuse before any Linux-specific step runs.
shim=$work/shim-other-platform
mkdir -p "$shim"
printf '#!/usr/bin/env bash\necho Darwin\n' >"$shim/uname"
chmod +x "$shim/uname"
out=$(sv PATH="$shim:$PATH" status 2>&1); st=$?
if [ "$st" -eq 1 ] && [[ "$out" == *"unsupported platform (Darwin)"* ]] && [[ "$out" == *"Linux only"* ]]; then
  ok "a non-Linux platform is refused at once, before any Linux-specific operation"
else
  bad "a non-Linux platform was not refused by the platform guard (exit $st)"; printf '%s\n' "$out" >&2
fi

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

# Stale-instance regression: a binary rebuilt after its instance started is
# reported. A future mtime stands in for the rebuild, so no timing is involved.
touch -d '+1 hour' "$bin_dir/slashit-ui"
out=$(sv verify 2>&1); expect fail "verify reports a binary rebuilt after its instance started" "$out" $? "STALE INSTANCE"
touch -d '2000-01-01 00:00:00' "$bin_dir/slashit-ui"
out=$(sv verify 2>&1); expect pass "verify passes again once the binary is no newer than the instance" "$out" $?

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

echo "== WebKit cache regeneration and UI preferences do not fail a restart; Task data still does"
webcache="$state/data/com.barradev.slashit-app/WebKitCache/Version 17/entry"
webprefs="$state/data/com.barradev.slashit-app/localstorage/http_localhost_1420.localstorage"
mkdir -p "$(dirname "$webcache")" "$(dirname "$webprefs")"
printf 'cache v1\n' >"$webcache"
printf 'prefs v1\n' >"$webprefs"
sv start >/dev/null 2>&1 || bad "webkit fixture: start failed"
out=$(sv SLASHIT_TEST_STARTUP_TOUCH="$webcache" restart 2>&1); status=$?
if [ "$status" -eq 0 ]; then ok "a regenerated WebKit cache entry does not fail the restart"
else bad "a WebKit cache rewrite failed the restart (exit $status)"; printf '%s\n' "$out" >&2; fi
cleanup_instances
sv start >/dev/null 2>&1 || bad "webkit prefs fixture: start failed"
out=$(sv SLASHIT_TEST_STARTUP_TOUCH="$webprefs" restart 2>&1); status=$?
if [ "$status" -eq 0 ] && [[ "$out" == *"UI preference files"* ]]; then ok "a rewritten UI preference file is reported, not failed"
else bad "a UI preference rewrite was not reported as expected (exit $status)"; printf '%s\n' "$out" >&2; fi
cleanup_instances
sv start >/dev/null 2>&1 || bad "task-data fixture: start failed"
out=$(sv SLASHIT_TEST_STARTUP_TOUCH="$state/data/slashit-app/board.toml" restart 2>&1); status=$?
if [ "$status" -eq 1 ] && [[ "$out" == *"state files changed during the restart"* ]]; then ok "a boot-time change to Task data is still a restart divergence"
else bad "a Task-data change was not refused (exit $status)"; printf '%s\n' "$out" >&2; fi
cleanup_instances
printf 'session 7\n' >"$state/data/slashit-app/board.toml"
rm -rf "$state/data/com.barradev.slashit-app"

echo "== frontend mode: an embedded build needs no server; a development build needs its dev server"
fe=$work/frontend
mkdir -p "$fe"
cp "$bin_dir/slashit-ui" "$fe/slashit-ui"
# Backdate the fixture binary: the stale-instance check compares whole-second
# mtimes with process age, so a binary written in the same second as its
# instance's start reads as rebuilt-after-start (a race, seen on CI).
touch -d '2000-01-01 00:00:00' "$fe/slashit-ui"
fe_sha=$(sha256sum "$fe/slashit-ui" | cut -d' ' -f1)
fe_dead=http://127.0.0.1:9/
printf 'mode=embedded\nsha256=%s\nwasm=x_bg.wasm\n' "$fe_sha" >"$fe/slashit-ui.frontend"

out=$(sv SLASHIT_DOGFOOD_BINARY="$fe/slashit-ui" SLASHIT_DOGFOOD_DEV_URL="$fe_dead" start 2>&1)
expect pass "an embedded build starts with no dev server answering" "$out" $?
out=$(sv SLASHIT_DOGFOOD_BINARY="$fe/slashit-ui" SLASHIT_DOGFOOD_DEV_URL="$fe_dead" verify 2>&1); st=$?
if [ "$st" -eq 0 ] && [[ "$out" == *"frontend:       embedded in the binary"* ]]; then ok "verify reports the embedded frontend, not a server"
else bad "verify did not report the embedded frontend (exit $st)"; printf '%s\n' "$out" >&2; fi
cleanup_instances

printf 'mode=embedded\nsha256=0000000000000000000000000000000000000000000000000000000000000000\n' >"$fe/slashit-ui.frontend"
out=$(sv SLASHIT_DOGFOOD_BINARY="$fe/slashit-ui" SLASHIT_DOGFOOD_DEV_URL="$fe_dead" start 2>&1)
expect fail "a sidecar whose sha256 does not match the binary is refused" "$out" $? "does not match the binary's sha256"
cleanup_instances

rm -f "$fe/slashit-ui.frontend"
out=$(sv SLASHIT_DOGFOOD_BINARY="$fe/slashit-ui" SLASHIT_DOGFOOD_DEV_URL="$fe_dead" start 2>&1)
expect fail "a development build without its dev server is refused, not started blank" "$out" $? "this is a development build"
cleanup_instances
# A running instance whose frontend record is then lost (a development build
# now, with no server) must be reported as degraded, not healthy.
printf 'mode=embedded\nsha256=%s\nwasm=x_bg.wasm\n' "$fe_sha" >"$fe/slashit-ui.frontend"
sv SLASHIT_DOGFOOD_BINARY="$fe/slashit-ui" SLASHIT_DOGFOOD_DEV_URL="$fe_dead" start >/dev/null 2>&1 || bad "degraded-verify fixture: start failed"
rm -f "$fe/slashit-ui.frontend"
out=$(sv SLASHIT_DOGFOOD_BINARY="$fe/slashit-ui" SLASHIT_DOGFOOD_DEV_URL="$fe_dead" verify 2>&1); st=$?
if [ "$st" -eq 1 ] && [[ "$out" == *"UNAVAILABLE"* ]]; then ok "verify is degraded when a running development build has no dev server"
else bad "verify did not report the degraded development frontend (exit $st)"; printf '%s\n' "$out" >&2; fi
cleanup_instances

if command -v python3 >/dev/null 2>&1 && command -v curl >/dev/null 2>&1; then
  # A free port picked at run time, and readiness only once this server
  # answers on it while its own process is alive: an unrelated listener on a
  # fixed port can neither be bound over nor be mistaken for ours.
  fe_port=$(python3 -c 'import socket; s = socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1]); s.close()') ||
    { bad "cannot pick a free port for the test dev server"; fe_port=; }
  fe_url="http://127.0.0.1:$fe_port/"
  fe_ready=0
  if [ -n "$fe_port" ]; then
    (cd "$fe" && exec python3 -m http.server "$fe_port" --bind 127.0.0.1 >/dev/null 2>&1) &
    fe_srv=$!
    for _ in $(seq 1 100); do
      kill -0 "$fe_srv" 2>/dev/null || break
      curl -fsS --max-time 1 -o /dev/null "$fe_url" 2>/dev/null && { fe_ready=1; break; }
      sleep 0.1
    done
  fi
  if [ "$fe_ready" = 1 ]; then
    out=$(sv SLASHIT_DOGFOOD_BINARY="$fe/slashit-ui" SLASHIT_DOGFOOD_DEV_URL="$fe_url" start 2>&1)
    expect pass "a development build starts once its dev server answers" "$out" $?
    out=$(sv SLASHIT_DOGFOOD_BINARY="$fe/slashit-ui" SLASHIT_DOGFOOD_DEV_URL="$fe_url" verify 2>&1)
    st=$?
    if [ "$st" -eq 0 ] && [[ "$out" == *"development server answers"* ]]; then ok "verify reports a ready dev server"
    else bad "verify did not report the ready dev server (exit $st)"; printf '%s\n' "$out" >&2; fi
  else
    bad "the test dev server did not answer on $fe_url while its process was alive"
  fi
  cleanup_instances
  if [ -n "$fe_srv" ]; then
    kill "$fe_srv" 2>/dev/null || true
    wait "$fe_srv" 2>/dev/null || true
    fe_srv=
  fi
else
  ok "dev-server readiness positive case skipped: python3 is not installed"
fi

echo "== restart snapshots are bounded to the newest five"
cleanup_instances
sv start >/dev/null 2>&1 || bad "retention fixture: start failed"
restarts_ok=0
for i in 1 2 3 4 5 6; do
  out=$(sv restart 2>&1) || { bad "restart $i of 6 failed"; printf '%s\n' "$out" >&2; break; }
  restarts_ok=$((restarts_ok + 1))
done
# Count backup directories by glob, not by parsing ls output.
kept=0
for dir in "$state"/supervisor/backup/[0-9]*/; do
  [ -d "$dir" ] && kept=$((kept + 1))
done
if [ "$restarts_ok" -eq 6 ] && [ "$kept" -eq 5 ]; then ok "six successful restarts leave exactly five snapshots"
else bad "after $restarts_ok successful restarts, $kept snapshots remain; expected exactly 5 after 6"; fi
cleanup_instances

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
mkdir -p "$disc/main/target/dogfood" "$disc/main/scripts"
cp "$bin_dir/slashit-ui" "$disc/main/target/dogfood/slashit-ui"
cp "$stub_dir/slashit" "$disc/main/target/dogfood/slashit"
cp "$sup" "$disc/main/scripts/supervise-dogfood.sh"
git -C "$disc/main" init -q
git -C "$disc/main" config user.name "Supervise Test"
git -C "$disc/main" config user.email supervise-test@example.invalid
git -C "$disc/main" add scripts/supervise-dogfood.sh
git -C "$disc/main" commit -q -m layout
d_env=(SLASHIT_DOGFOOD_STATE="$state" SLASHIT_DOGFOOD_RUNTIME="$runtime"
  XDG_CONFIG_HOME= XDG_DATA_HOME= XDG_CACHE_HOME= XDG_STATE_HOME= XDG_RUNTIME_DIR= GH_CONFIG_DIR=)
out=$(cd / && env "${d_env[@]}" timeout 60 bash "$disc/main/scripts/supervise-dogfood.sh" env 2>&1); st=$?
if [ "$st" -eq 0 ] && grep -qxE "BINARY +$disc/main/target/dogfood/slashit-ui" <<<"$out" &&
  grep -qxE "CLI +$disc/main/target/dogfood/slashit" <<<"$out"; then
  ok "default discovery finds the binary and the CLI beside it, with no overrides"
else
  bad "default discovery did not resolve the checkout layout (exit $st)"; printf '%s\n' "$out" >&2
fi

git -C "$disc/main" worktree add -q "$disc/task" -b task-probe >/dev/null 2>&1
out=$(cd / && env "${d_env[@]}" timeout 60 bash "$disc/task/scripts/supervise-dogfood.sh" env 2>&1); st=$?
if [ "$st" -eq 0 ] && grep -qxE "BINARY +$disc/main/target/dogfood/slashit-ui" <<<"$out"; then
  ok "discovery from a linked worktree (the Task Checkout geometry) resolves the main checkout's build"
else
  bad "discovery from a linked worktree did not resolve the main checkout (exit $st)"; printf '%s\n' "$out" >&2
fi

rm -f "$disc/main/target/dogfood/slashit"
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
# Hold the embedded-frontend sidecar aside so the frontend is a development
# build with no server (degraded). The environment verdict must still report.
mv "$bin_dir/slashit-ui.frontend" "$bin_dir/slashit-ui.frontend.held"
out=$(sv status 2>&1); st=$?
if [ "$st" -eq 4 ] && [[ "$out" == *"MISMATCH"* ]] && [[ "$out" == *"UNAVAILABLE"* ]]; then
  ok "status exits 4 for a mismatched environment even when the frontend is degraded"
else
  bad "status did not exit 4 with the mismatch and the degraded frontend both reported (exit $st)"; printf '%s\n' "$out" >&2
fi
out=$(sv verify 2>&1); st=$?
mv "$bin_dir/slashit-ui.frontend.held" "$bin_dir/slashit-ui.frontend"
if [ "$st" -eq 4 ] && [[ "$out" == *"MISMATCH"* ]] && [[ "$out" == *"UNAVAILABLE"* ]]; then
  ok "verify exits 4 and reports the environment mismatch even when the frontend is degraded"
else
  bad "verify did not exit 4 for an environment mismatch (exit $st)"; printf '%s\n' "$out" >&2
fi
cleanup_instances

echo "== an endpoint path with no verified answer is unverifiable, never stopped"
cleanup_instances
mkdir -p "$runtime/slashit-app"
: >"$socket"
out=$(sv status 2>&1); st=$?
if [ "$st" -eq 3 ] && [[ "$out" == *"cannot verify"* ]] && [[ "$out" != *"instance:       stopped"* ]]; then
  ok "status exits 3 for an endpoint path whose answer cannot be verified (never reports stopped)"
else
  bad "status reported a socket with no verified answer as stopped or as success (exit $st)"; printf '%s\n' "$out" >&2
fi
out=$(sv verify 2>&1); st=$?
if [ "$st" -eq 3 ] && [[ "$out" == *"cannot verify"* ]]; then
  ok "verify exits 3 for the same endpoint path"
else
  bad "verify did not exit 3 for an endpoint path with no verified answer (exit $st)"; printf '%s\n' "$out" >&2
fi
rm -f "$socket"

echo "== stop --force escalates to signals when there is no CLI"
out=$(sv start 2>&1); st=$?
forced_pid=$(stub_pid)
if [ "$st" -eq 0 ] && [ -n "$forced_pid" ]; then
  out=$(sv SLASHIT_DOGFOOD_CLI= SLASHIT_DOGFOOD_STOP_TIMEOUT=2 stop --force 2>&1); st=$?
  if [ "$st" -eq 0 ] && ! kill -0 "$forced_pid" 2>/dev/null; then
    ok "stop --force without a CLI escalates to signals and ends the instance"
  else
    bad "stop --force without a CLI did not end the instance (exit $st)"; printf '%s\n' "$out" >&2
  fi
else
  bad "stop --force fixture: start failed (exit $st)"; printf '%s\n' "$out" >&2
fi
cleanup_instances

echo "== a forced stop leaves ambiguous endpoint cleanup to the owner"
out=$(sv SLASHIT_TEST_STUB_ROLE=noquit start 2>&1); st=$?
forced_pid=$(stub_pid)
if [ "$st" -eq 0 ] && [ -n "$forced_pid" ]; then
  out=$(sv SLASHIT_DOGFOOD_STOP_TIMEOUT=2 SLASHIT_DOGFOOD_FORCE_TIMEOUT=2 stop --force 2>&1); st=$?
  if [ "$st" -eq 0 ] && ! kill -0 "$forced_pid" 2>/dev/null && [ -e "$socket" ] &&
    [[ "$out" == *"left endpoint path $socket in place"* ]]; then
    out=$(sv start 2>&1); st=$?
    if [ "$st" -eq 1 ] && [[ "$out" == *"endpoint path $socket exists"* ]] && [ -e "$socket" ]; then
      ok "after forced stop, the ambiguous endpoint is preserved and start refuses with cleanup guidance"
    else
      bad "start did not fail closed on the preserved endpoint (exit $st)"; printf '%s\n' "$out" >&2
    fi
  else
    bad "stop --force did not end the instance and preserve the endpoint with guidance (exit $st)"; printf '%s\n' "$out" >&2
  fi
else
  bad "forced-stop fixture: start failed (exit $st)"; printf '%s\n' "$out" >&2
fi
cleanup_instances

echo "== a replacement endpoint is never unlinked after the recorded process exits"
out=$(sv SLASHIT_TEST_STUB_ROLE=noquit start 2>&1); st=$?
foreign_pid=$(stub_pid)
if [ "$st" -eq 0 ] && [ -n "$foreign_pid" ]; then
  printf 'foreign listener replacement\n' >"$socket"
  out=$(sv SLASHIT_DOGFOOD_FORCE_TIMEOUT=2 stop --force 2>&1); st=$?
  if [ "$st" -eq 0 ] && ! kill -0 "$foreign_pid" 2>/dev/null &&
    [ "$(cat "$socket" 2>/dev/null)" = 'foreign listener replacement' ] &&
    [[ "$out" == *"left endpoint path $socket in place"* ]]; then
    ok "a foreign replacement at the endpoint survives stop and is reported for manual inspection"
  else
    bad "stop removed or failed to diagnose the replacement endpoint (exit $st)"; printf '%s\n' "$out" >&2
  fi
else
  bad "replacement-endpoint fixture: start failed (exit $st)"; printf '%s\n' "$out" >&2
fi
cleanup_instances

echo "== a stop over a control channel that does not answer in time is refused as unverifiable"
out=$(sv start 2>&1); st=$?
hung_pid=$(stub_pid)
if [ "$st" -eq 0 ] && [ -n "$hung_pid" ]; then
  out=$(sv SLASHIT_TEST_CLI_HANG=1 SLASHIT_DOGFOOD_CLI_TIMEOUT=2 stop 2>&1); st=$?
  if [ "$st" -eq 1 ] && [[ "$out" == *"did not answer within 2s"* ]] && kill -0 "$hung_pid" 2>/dev/null; then
    ok "stop refuses with the cannot-verify reason when the ping times out, and the instance keeps running"
  else
    bad "stop over a hung control channel was not refused as unverifiable (exit $st)"; printf '%s\n' "$out" >&2
  fi
else
  bad "hung-channel fixture: start failed (exit $st)"; printf '%s\n' "$out" >&2
fi
cleanup_instances

echo "== run refuses a binary that prints a version line and then fails, as start does"
printf '#!/usr/bin/env bash\necho "slashit-ui 9.9.9-broken"\nexit 3\n' >"$work/broken-version"
chmod +x "$work/broken-version"
out=$(sv SLASHIT_DOGFOOD_BINARY="$work/broken-version" run 2>&1); st=$?
if [ "$st" -eq 1 ] && [[ "$out" == *"does not answer --version"* ]] && [[ "$out" != *"exec "* ]]; then
  ok "run refuses a binary whose --version exits nonzero, even when it printed a line"
else
  bad "run accepted a binary whose --version exits nonzero (exit $st)"; printf '%s\n' "$out" >&2
fi

echo "== run requires a present CLI with a successful, nonempty version"
out=$(sv SLASHIT_DOGFOOD_CLI= run 2>&1); st=$?
if [ "$st" -eq 1 ] && [[ "$out" == *'no `slashit` CLI is available'* ]] && [[ "$out" != *"exec "* ]]; then
  ok "run refuses when the provenance CLI is missing"
else
  bad "run did not refuse the missing provenance CLI (exit $st)"; printf '%s\n' "$out" >&2
fi
printf '#!/usr/bin/env bash\nexit 0\n' >"$work/empty-version-cli"
chmod +x "$work/empty-version-cli"
out=$(sv SLASHIT_DOGFOOD_CLI="$work/empty-version-cli" run 2>&1); st=$?
if [ "$st" -eq 1 ] && [[ "$out" == *"does not answer --version"* ]] && [[ "$out" != *"exec "* ]]; then
  ok "run refuses a CLI whose --version succeeds without version output"
else
  bad "run accepted an empty CLI version (exit $st)"; printf '%s\n' "$out" >&2
fi
printf '#!/usr/bin/env bash\necho "slashit cli 9.9.9-broken"\nexit 3\n' >"$work/failing-version-cli"
chmod +x "$work/failing-version-cli"
out=$(sv SLASHIT_DOGFOOD_CLI="$work/failing-version-cli" run 2>&1); st=$?
if [ "$st" -eq 1 ] && [[ "$out" == *"does not answer --version"* ]] && [[ "$out" != *"exec "* ]]; then
  ok "run refuses a CLI whose --version prints a line and exits nonzero"
else
  bad "run accepted a failing CLI version check (exit $st)"; printf '%s\n' "$out" >&2
fi

echo "== with no launch record, a control channel that does not answer is still unverifiable, not absent"
cleanup_instances
out=$(sv SLASHIT_TEST_CLI_HANG=1 SLASHIT_DOGFOOD_CLI_TIMEOUT=2 stop 2>&1); st=$?
if [ "$st" -eq 1 ] && [[ "$out" == *"did not answer within 2s"* ]] && [[ "$out" != *"nothing found"* ]]; then
  ok "stop with no record and a hung channel says it cannot verify, not that nothing is running"
else
  bad "stop with no record and a hung channel did not report an unverifiable channel (exit $st)"; printf '%s\n' "$out" >&2
fi

echo "== a restart that could not start again is refused before the instance is stopped"
out=$(sv start 2>&1); st=$?
running_pid=$(stub_pid)
if [ "$st" -eq 0 ] && [ -n "$running_pid" ]; then
  out=$(sv SLASHIT_DOGFOOD_CLI= restart --force 2>&1); st=$?
  if [ "$st" -eq 1 ] && [[ "$out" == *"CLI is available"* ]] && kill -0 "$running_pid" 2>/dev/null; then
    ok "restart --force with no CLI is refused before the stop; the instance keeps running"
  else
    bad "restart --force with no CLI stopped the instance or did not refuse (exit $st)"; printf '%s\n' "$out" >&2
  fi
else
  bad "restart fixture: start failed (exit $st)"; printf '%s\n' "$out" >&2
fi
cleanup_instances

echo "== process identity: a launch record alone never authorizes a signal"
# Foreign processes are disposable `sleep` children of this test. The EXIT trap
# kills any that remain; no case signals a pid this test did not start itself.
is_alive() { # pid: exists and has not exited (a zombie has exited)
  local stat rest
  stat=$(cat "/proc/$1/stat" 2>/dev/null) || return 1
  rest=${stat##*) }
  case $rest in Z | "Z "*) return 1 ;; esac
  return 0
}
boot_id_now=$(cat /proc/sys/kernel/random/boot_id)
record_file="$state/supervisor/instance"
expect_refused_alive() { # label output status reason pid
  local label=$1 out=$2 st=$3 reason=$4 pid=$5
  if [ "$st" -eq 1 ] && [[ "$out" == *"$reason"* ]] && is_alive "$pid"; then
    ok "$label (refused: $reason; pid $pid still alive)"
  else
    bad "$label: expected exit 1 with '$reason' and pid $pid alive, exited $st alive=$(is_alive "$pid" && echo yes || echo no)"
    printf '%s\n' "$out" >&2
  fi
}

sleep 300 &
foreign_pid=$!
own "$foreign_pid"
foreign_start=$(proc_start "$foreign_pid")

# A stale record that names the unrelated live pid, with the identity of an
# instance that no longer exists (a different start time).
printf 'pid\t%s\nboot_id\t%s\nstart_ticks\t%s\n' "$foreign_pid" "$boot_id_now" 1 >"$record_file"
out=$(sv stop 2>&1); st=$?
expect_refused_alive "a stop refuses a stale record that names an unrelated live pid" "$out" "$st" "now belongs to a different process" "$foreign_pid"
out=$(sv SLASHIT_DOGFOOD_STOP_TIMEOUT=2 SLASHIT_DOGFOOD_FORCE_TIMEOUT=2 stop --force 2>&1); st=$?
expect_refused_alive "a forced stop refuses that foreign pid and signals nothing" "$out" "$st" "now belongs to a different process" "$foreign_pid"
out=$(sv SLASHIT_DOGFOOD_STOP_TIMEOUT=2 SLASHIT_DOGFOOD_FORCE_TIMEOUT=2 restart --force 2>&1); st=$?
expect_refused_alive "a forced restart refuses that foreign pid and stops nothing" "$out" "$st" "now belongs to a different process" "$foreign_pid"

# The pid is alive and in this boot, but the start time differs by one tick.
printf 'pid\t%s\nboot_id\t%s\nstart_ticks\t%s\n' "$foreign_pid" "$boot_id_now" $((foreign_start + 1)) >"$record_file"
out=$(sv SLASHIT_DOGFOOD_STOP_TIMEOUT=2 SLASHIT_DOGFOOD_FORCE_TIMEOUT=2 stop --force 2>&1); st=$?
expect_refused_alive "a live pid with a mismatched start time is not signaled" "$out" "$st" "now belongs to a different process" "$foreign_pid"

# The same pid recorded under a previous boot: its process cannot still exist.
printf 'pid\t%s\nboot_id\t%s\nstart_ticks\t%s\n' "$foreign_pid" "00000000-0000-0000-0000-000000000000" "$foreign_start" >"$record_file"
out=$(sv SLASHIT_DOGFOOD_STOP_TIMEOUT=2 SLASHIT_DOGFOOD_FORCE_TIMEOUT=2 stop --force 2>&1); st=$?
expect_refused_alive "a record from a previous boot is not signaled" "$out" "$st" "now belongs to a different process" "$foreign_pid"

# Missing identity: the record names a live pid but proves nothing about it.
printf 'pid\t%s\n' "$foreign_pid" >"$record_file"
out=$(sv SLASHIT_DOGFOOD_STOP_TIMEOUT=2 SLASHIT_DOGFOOD_FORCE_TIMEOUT=2 stop --force 2>&1); st=$?
expect_refused_alive "a record without process identity is refused under --force" "$out" "$st" "has no recorded process identity" "$foreign_pid"
out=$(sv SLASHIT_DOGFOOD_STOP_TIMEOUT=2 SLASHIT_DOGFOOD_FORCE_TIMEOUT=2 restart --force 2>&1); st=$?
expect_refused_alive "a forced restart refuses a record without process identity" "$out" "$st" "has no recorded process identity" "$foreign_pid"

# Malformed identity: a start time that is not a number.
printf 'pid\t%s\nboot_id\t%s\nstart_ticks\tabc\n' "$foreign_pid" "$boot_id_now" >"$record_file"
out=$(sv SLASHIT_DOGFOOD_STOP_TIMEOUT=2 SLASHIT_DOGFOOD_FORCE_TIMEOUT=2 stop --force 2>&1); st=$?
expect_refused_alive "a record with a malformed start time is refused under --force" "$out" "$st" "has a malformed process identity" "$foreign_pid"

# Malformed pid: not a number at all.
printf 'pid\t%s\nboot_id\t%s\nstart_ticks\t%s\n' "12x" "$boot_id_now" "$foreign_start" >"$record_file"
out=$(sv SLASHIT_DOGFOOD_STOP_TIMEOUT=2 SLASHIT_DOGFOOD_FORCE_TIMEOUT=2 stop --force 2>&1); st=$?
expect_refused_alive "a record with a non-numeric pid is refused under --force" "$out" "$st" "is not a valid pid" "$foreign_pid"

# A status over the same stale record must not call the foreign pid the instance.
printf 'pid\t%s\nboot_id\t%s\nstart_ticks\t%s\n' "$foreign_pid" "$boot_id_now" 1 >"$record_file"
out=$(sv status 2>&1); st=$?
if [[ "$out" == *"stale (it names pid $foreign_pid, which now belongs to a different process)"* ]] && [[ "$out" != *"STILL ALIVE"* ]] && is_alive "$foreign_pid"; then
  ok "status reports a reused pid as stale, not as the still-running instance"
else
  bad "status misreported a reused pid (exit $st)"; printf '%s\n' "$out" >&2
fi

# Positive control: the same process, with its verified identity, IS signaled.
# This proves the refusals above come from the identity check, not from a
# supervisor that never signals a recorded pid.
env "${declared_env[@]}" sleep 300 &
control_pid=$!
own "$control_pid"
control_start=$(proc_start "$control_pid")
printf 'pid\t%s\nboot_id\t%s\nstart_ticks\t%s\n' "$control_pid" "$boot_id_now" "$control_start" >"$record_file"
out=$(sv SLASHIT_DOGFOOD_STOP_TIMEOUT=2 SLASHIT_DOGFOOD_FORCE_TIMEOUT=2 stop --force 2>&1); st=$?
wait "$control_pid" 2>/dev/null
if [ "$st" -eq 0 ] && [[ "$out" == *"ended after signal escalation"* ]] && ! is_alive "$control_pid"; then
  ok "a forced stop of a pid whose recorded identity matches is signaled and ends it"
else
  bad "the identity-verified control was not stopped (exit $st)"; printf '%s\n' "$out" >&2
fi

echo "== process identity: an endpoint that disagrees with the launch record"
cleanup_instances
out=$(sv SLASHIT_TEST_STUB_ROLE=noquit start 2>&1); st=$?
live_instance=$(stub_pid)
if [ "$st" -eq 0 ] && [ -n "$live_instance" ]; then
  printf 'pid\t%s\nboot_id\t%s\nstart_ticks\t%s\n' "$foreign_pid" "$boot_id_now" "$foreign_start" >"$record_file"
  out=$(sv SLASHIT_DOGFOOD_STOP_TIMEOUT=2 SLASHIT_DOGFOOD_FORCE_TIMEOUT=2 stop --force 2>&1); st=$?
  if [ "$st" -eq 1 ] && [[ "$out" == *"the endpoint is served by pid $live_instance"* ]] && is_alive "$foreign_pid" && is_alive "$live_instance"; then
    ok "a forced stop refuses when the endpoint and the record disagree; both processes survive"
  else
    bad "a disagreeing endpoint was not refused with both processes alive (exit $st)"; printf '%s\n' "$out" >&2
  fi
else
  bad "disagreement fixture: start failed (exit $st)"; printf '%s\n' "$out" >&2
fi
cleanup_instances
rm -f "$record_file"

echo "== endpoint and launch record name different processes: nothing is quit or signaled"
# The endpoint answers for one live instance (A). The launch record names a second
# live process (B) with a matching identity and the declared environment. Each
# process passes its own checks, so only the disagreement can refuse.
cleanup_instances
out=$(sv SLASHIT_TEST_STUB_ROLE=noquit start 2>&1); st=$?
endpoint_pid=$(stub_pid)
env "${declared_env[@]}" sleep 300 &
record_pid=$!
own "$record_pid"
record_id="$record_pid $(proc_start "$record_pid")"
if [ "$st" -eq 0 ] && [ -n "$endpoint_pid" ] && is_alive "$record_pid"; then
  printf 'pid\t%s\nboot_id\t%s\nstart_ticks\t%s\n' "$record_pid" "$boot_id_now" "$(proc_start "$record_pid")" >"$record_file"
  # Refused before any quit, TERM or KILL: both processes survive, and the
  # refusal names the disagreement so the operator can act on it.
  disagree_case() { # label, verb arguments...
    local label=$1
    shift
    : >"$stub_dir/calls"
    out=$(sv SLASHIT_DOGFOOD_STOP_TIMEOUT=2 SLASHIT_DOGFOOD_FORCE_TIMEOUT=2 "$@" 2>&1); st=$?
    if [ "$st" -eq 1 ] && [[ "$out" == *"the launch record names pid $record_pid"* ]] &&
      is_alive "$endpoint_pid" && is_alive "$record_pid" && ! grep -qx quit "$stub_dir/calls" 2>/dev/null; then
      ok "$label (refused: the endpoint is served by pid $endpoint_pid, the record names pid $record_pid; both alive, no quit)"
    else
      bad "$label: expected a refusal naming pid $record_pid, both processes alive and no quit (exit $st)"; printf '%s\n' "$out" >&2
    fi
  }
  disagree_case "a forced stop refuses an endpoint and a record that name different processes" stop --force
  disagree_case "a forced restart refuses the same disagreement" restart --force
  disagree_case "a graceful stop refuses the same disagreement" stop
else
  bad "disagreement fixture: start failed or a process was missing (exit $st)"; printf '%s\n' "$out" >&2
fi
cleanup_instances
rm -f "$record_file"
signal_owned KILL "$record_id" || true
wait "$record_pid" 2>/dev/null || true

echo "== the identity re-read after the environment check refuses a changed process"
# Deterministic, without pid reuse: PATH shims stand in for two commands. `tr` is
# the only command verify_env runs, so once it has run, the `cat` shim reports
# the endpoint pid's start time one tick later. The re-read must see that and
# refuse before any quit. Without the re-read, the stop would send quit.
cleanup_instances
out=$(sv SLASHIT_TEST_STUB_ROLE=noquit start 2>&1); st=$?
reread_pid=$(stub_pid)
if [ "$st" -eq 0 ] && [ -n "$reread_pid" ]; then
  reread_shim=$work/reread-shim
  reread_flag=$work/reread-flag
  mkdir -p "$reread_shim"
  real_tr=$(command -v tr)
  real_cat=$(command -v cat)
  cat >"$reread_shim/tr" <<SHIM
#!/usr/bin/env bash
: >"$reread_flag"
exec "$real_tr" "\$@"
SHIM
  cat >"$reread_shim/cat" <<SHIM
#!/usr/bin/env bash
if [ -e "$reread_flag" ] && [ "\$1" = "/proc/$reread_pid/stat" ]; then
  stat=\$("$real_cat" -- "\$1") || exit 1
  pre=\${stat%) *}
  rest=\${stat##*) }
  read -r -a fields <<<"\$rest"
  fields[19]=\$((fields[19] + 1))
  printf '%s) %s\n' "\$pre" "\${fields[*]}"
  exit 0
fi
exec "$real_cat" "\$@"
SHIM
  chmod +x "$reread_shim/tr" "$reread_shim/cat"
  rm -f "$reread_flag"
  : >"$stub_dir/calls"
  out=$(sv PATH="$reread_shim:$PATH" SLASHIT_DOGFOOD_STOP_TIMEOUT=2 SLASHIT_DOGFOOD_FORCE_TIMEOUT=2 stop --force 2>&1); st=$?
  if [ "$st" -eq 1 ] && [[ "$out" == *"changed while it was being verified"* ]] && [ -e "$reread_flag" ] &&
    is_alive "$reread_pid" && ! grep -qx quit "$stub_dir/calls" 2>/dev/null; then
    ok "a forced stop refuses when the endpoint pid's start time changes during verification, with no quit (refused: changed while it was being verified)"
  else
    bad "the re-read did not refuse a changed endpoint pid before any quit (exit $st, verified-env reached: $([ -e "$reread_flag" ] && echo yes || echo no))"; printf '%s\n' "$out" >&2
  fi
else
  bad "re-read fixture: start failed (exit $st)"; printf '%s\n' "$out" >&2
fi
cleanup_instances

echo "== process identity: a verified instance still stops and restarts"
out=$(sv SLASHIT_TEST_STUB_ROLE=noquit start 2>&1); st=$?
first_pid=$(stub_pid)
if [ "$st" -eq 0 ] && [ -n "$first_pid" ] && grep -q "^boot_id	$boot_id_now\$" "$record_file" && grep -q "^start_ticks	" "$record_file"; then
  ok "start records the verified identity (boot id and start time) of the launched instance"
else
  bad "start did not record the process identity (exit $st)"; printf '%s\n' "$out" >&2
fi
out=$(sv SLASHIT_DOGFOOD_STOP_TIMEOUT=2 SLASHIT_DOGFOOD_FORCE_TIMEOUT=2 restart --force 2>&1); st=$?
if [ "$st" -eq 1 ] && [[ "$out" == *"ended after signal escalation"* ]] && ! is_alive "$first_pid" &&
  [[ "$out" == *"the endpoint path"*"nothing could be verified"* ]]; then
  ok "a forced restart ends only the verified instance, then refuses to start over the socket its forced stop left"
else
  bad "forced restart of the verified instance did not end it and refuse the stale endpoint (exit $st)"; printf '%s\n' "$out" >&2
fi
cleanup_instances

echo "== process identity: recovery after the recorded instance has exited"
out=$(sv SLASHIT_TEST_STUB_ROLE=noquit start 2>&1); st=$?
dead_instance=$(stub_pid)
if [ "$st" -eq 0 ] && [ -n "$dead_instance" ]; then
  owned_by_fixture "$dead_instance" && kill -9 "$dead_instance" 2>/dev/null
  for _ in $(seq 1 50); do is_alive "$dead_instance" || break; sleep 0.1; done
  out=$(sv status 2>&1); st=$?
  if [ "$st" -eq 3 ] && [[ "$out" == *"stale (it names pid $dead_instance"* ]]; then
    ok "status after an unexpected exit reports the record as stale and the endpoint as unverifiable"
  else
    bad "status after an unexpected exit (exit $st)"; printf '%s\n' "$out" >&2
  fi
  out=$(sv stop 2>&1); st=$?
  if [ "$st" -eq 1 ] && [[ "$out" == *"nothing found to operate on"* ]] && [[ "$out" == *"so it is not signaled"* ]]; then
    ok "stop after an unexpected exit has nothing to signal and refuses"
  else
    bad "stop after an unexpected exit did not refuse with nothing to signal (exit $st)"; printf '%s\n' "$out" >&2
  fi
  out=$(sv start 2>&1); st=$?
  if [ "$st" -eq 1 ] && [[ "$out" == *"the endpoint path"* ]] && [[ "$out" == *"nothing could be verified"* ]]; then
    ok "start refuses while the stale endpoint path is unverified (the documented manual step)"
  else
    bad "start did not refuse the unverified endpoint path (exit $st)"; printf '%s\n' "$out" >&2
  fi
  rm -f "$socket"
  out=$(sv start 2>&1); st=$?
  new_instance=$(stub_pid)
  if [ "$st" -eq 0 ] && [ -n "$new_instance" ] && [ "$new_instance" != "$dead_instance" ]; then
    ok "after the stale endpoint is removed by hand, start launches a new identified instance"
  else
    bad "start did not recover after the manual endpoint removal (exit $st)"; printf '%s\n' "$out" >&2
  fi
else
  bad "recovery fixture: start failed (exit $st)"; printf '%s\n' "$out" >&2
fi
cleanup_instances

echo "== no launch record: an endpoint's own answer never authorizes a quit or a signal"
# The stub CLI prints a reply file for ping, so each case declares its own
# endpoint. The foreign processes are disposable: one carries no declared
# environment, the other carries all of it, so only mode or endpoint can refuse
# it.
cleanup_instances
rm -f "$record_file"
sleep 300 &
bare_pid=$!
own "$bare_pid"
env "${declared_env[@]}" sleep 300 &
declared_pid=$!
own "$declared_pid"
reply_file=$work/endpoint-reply.json
reply() { # mode endpoint pid
  printf '{\n  "mode": "%s",\n  "version": "9.9.9-stub",\n  "protocol_version": 1,\n  "pid": %s,\n  "endpoint": "%s"\n}\n' "$1" "$3" "$2" >"$reply_file"
}
no_quit() { # label: the refused endpoint must not have received a quit
  if grep -qx quit "$stub_dir/calls" 2>/dev/null; then
    bad "$1: a quit reached the refused endpoint"
  else
    ok "$1: no quit reached the refused endpoint"
  fi
}

reply gui "unix:$socket" "$bare_pid"
: >"$stub_dir/calls"
out=$(sv SLASHIT_TEST_CLI_FAKE_PING="$reply_file" SLASHIT_DOGFOOD_STOP_TIMEOUT=2 SLASHIT_DOGFOOD_FORCE_TIMEOUT=2 stop --force 2>&1); st=$?
expect_refused_alive "a forced stop refuses an endpoint pid with no declared environment" "$out" "$st" "does not carry the declared" "$bare_pid"
no_quit "forced stop"

: >"$stub_dir/calls"
out=$(sv SLASHIT_TEST_CLI_FAKE_PING="$reply_file" SLASHIT_DOGFOOD_STOP_TIMEOUT=2 SLASHIT_DOGFOOD_FORCE_TIMEOUT=2 restart --force 2>&1); st=$?
expect_refused_alive "a forced restart refuses the same endpoint pid" "$out" "$st" "does not carry the declared" "$bare_pid"
no_quit "forced restart"

: >"$stub_dir/calls"
out=$(sv SLASHIT_TEST_CLI_FAKE_PING="$reply_file" SLASHIT_DOGFOOD_STOP_TIMEOUT=2 stop 2>&1); st=$?
expect_refused_alive "a graceful stop refuses the same endpoint pid" "$out" "$st" "does not carry the declared" "$bare_pid"
no_quit "graceful stop"

reply cli "unix:$socket" "$declared_pid"
: >"$stub_dir/calls"
out=$(sv SLASHIT_TEST_CLI_FAKE_PING="$reply_file" SLASHIT_DOGFOOD_STOP_TIMEOUT=2 SLASHIT_DOGFOOD_FORCE_TIMEOUT=2 stop --force 2>&1); st=$?
expect_refused_alive "a forced stop refuses an endpoint that declares mode cli" "$out" "$st" "as mode 'cli', not 'gui'" "$declared_pid"
no_quit "mode mismatch"

reply gui "unix:/elsewhere/other.sock" "$declared_pid"
: >"$stub_dir/calls"
out=$(sv SLASHIT_TEST_CLI_FAKE_PING="$reply_file" SLASHIT_DOGFOOD_STOP_TIMEOUT=2 SLASHIT_DOGFOOD_FORCE_TIMEOUT=2 restart --force 2>&1); st=$?
expect_refused_alive "a forced restart refuses an endpoint on another path" "$out" "$st" "not on the declared endpoint" "$declared_pid"
no_quit "endpoint mismatch"

cleanup_instances
sleep 300 &
rec_bare=$!
own "$rec_bare"
printf 'pid\t%s\nboot_id\t%s\nstart_ticks\t%s\n' "$rec_bare" "$boot_id_now" "$(proc_start "$rec_bare")" >"$record_file"
out=$(sv SLASHIT_DOGFOOD_STOP_TIMEOUT=2 SLASHIT_DOGFOOD_FORCE_TIMEOUT=2 stop --force 2>&1); st=$?
expect_refused_alive "a record whose identity matches a process without the declared environment is refused" "$out" "$st" "does not carry the declared" "$rec_bare"
rm -f "$record_file"

out=$(sv SLASHIT_TEST_STUB_ROLE=noquit start 2>&1); st=$?
unrecorded=$(stub_pid)
rm -f "$record_file"
: >"$stub_dir/calls"
if [ "$st" -eq 0 ] && [ -n "$unrecorded" ]; then
  out=$(sv SLASHIT_DOGFOOD_STOP_TIMEOUT=2 SLASHIT_DOGFOOD_FORCE_TIMEOUT=2 stop --force 2>&1); st=$?
  if [ "$st" -eq 0 ] && ! is_alive "$unrecorded" && grep -qx quit "$stub_dir/calls"; then
    ok "a forced stop of an instance with no launch record proceeds on its endpoint, mode and environment"
  else
    bad "a forced stop of an unrecorded instance did not end it on verified evidence (exit $st)"; printf '%s\n' "$out" >&2
  fi
else
  bad "unrecorded-instance fixture: start failed (exit $st)"; printf '%s\n' "$out" >&2
fi
cleanup_instances

echo "== a failed start leaves no untracked child and no second launch"
# Each case marks its supervisor and that supervisor's children with a unique
# environment variable, so the children are found even when no pid is known.
marked() { # marker: the pids of live processes whose environment carries it
  local f
  for f in /proc/[0-9]*/environ; do
    case $'\n'"$({ tr '\0' '\n' <"$f"; } 2>/dev/null)"$'\n' in
      *$'\n'"SLASHIT_TEST_MARK=$1"$'\n'*) echo "${f#/proc/}" ;;
    esac
  done
}

# A killed supervisor can leave a helper that inherited its lifecycle lock for a
# moment (a readiness-loop sleep, at most a fraction of a second). The lock is
# then busy for a start, which refuses it: fail-closed. The cases wait for it.
wait_lock_free() {
  local i
  for i in $(seq 100); do
    flock -n "$state/supervisor/lifecycle.lock" true 2>/dev/null && return 0
    sleep 0.1
  done
  return 1
}

cleanup_instances
rm -f "$record_file"
m_never=never-$$
out=$(sv SLASHIT_TEST_MARK=$m_never SLASHIT_TEST_CLI_NEVER_ANSWERS=1 SLASHIT_TEST_STUB_ROLE=noquit SLASHIT_DOGFOOD_START_TIMEOUT=2 start 2>&1); st=$?
if [ "$st" -eq 1 ] && [[ "$out" == *"no answer on"* ]] && [[ "$out" == *"the started process has ended"* ]] && [ -z "$(marked "$m_never")" ]; then
  ok "a start whose CLI never answers ends the child it spawned (refused: no answer on the endpoint)"
else
  bad "a start whose CLI never answers did not end its child (exit $st)"; printf '%s\n' "$out" >&2
fi
if grep -q "^state[[:space:]]provisional" "$record_file" 2>/dev/null; then
  ok "the failed start keeps its provisional record"
else
  bad "the failed start kept no provisional record"
fi
out=$(sv start 2>&1); st=$?
if [ "$st" -eq 1 ] && [[ "$out" == *"exists but nothing could be verified"* ]] && [[ "$out" == *"$socket"* ]]; then
  ok "the next start refuses the endpoint path the ended child left, until an operator removes it"
else
  bad "the next start did not refuse the leftover endpoint path (exit $st)"; printf '%s\n' "$out" >&2
fi
rm -f "$socket"
out=$(sv start 2>&1); st=$?
second=$(stub_pid)
if [ "$st" -eq 0 ] && [ -n "$second" ] && is_alive "$second" && grep -q "^state[[:space:]]verified" "$record_file"; then
  ok "after the path is removed by hand, one instance starts with a verified record"
else
  bad "the start after the failed one did not launch cleanly (exit $st)"; printf '%s\n' "$out" >&2
fi
cleanup_instances

# A supervisor killed while it waits for readiness leaves the provisional record
# over a live child. Sets crash_child and crash_marker for the case. Only the
# supervisor that this fixture's background start became is killed: it is found
# from that job's process tree and signaled by pid and start time. No pattern is
# matched against command lines, so an unrelated process cannot be selected.
crash_mid_start() {
  cleanup_instances
  rm -f "$record_file"
  crash_marker=crash-$RANDOM-$$
  sv SLASHIT_TEST_MARK=$crash_marker SLASHIT_TEST_CLI_NEVER_ANSWERS=1 SLASHIT_TEST_STUB_ROLE=noquit SLASHIT_DOGFOOD_START_TIMEOUT=120 start >/dev/null 2>&1 &
  local svjob=$! i sup_info
  for i in $(seq 150); do
    if grep -q "^state[[:space:]]provisional" "$record_file" 2>/dev/null && [ -s "$stub_dir/pid" ]; then break; fi
    sleep 0.1
  done
  sup_info=$(fixture_supervisor "$svjob") || {
    bad "crash fixture: the supervisor of the background start could not be identified; nothing was killed"
    crash_child=
    return 1
  }
  signal_owned KILL "$sup_info"
  wait "$svjob" 2>/dev/null
  # The interrupted child has not bound its endpoint yet, so the path is absent.
  rm -f "$socket"
  wait_lock_free
  crash_child=$(awk -F '\t' '$1 == "pid" { print $2 }' "$record_file" 2>/dev/null | head -n 1)
}

# A look-alike process: its command line contains the supervisor path and
# `start`, which a broad pattern match would select, but it is not this
# fixture's supervisor. It must survive the crash fixture. The harness started
# it, so it is ended below by pid and start time.
bash -c 'exec -a "$0" sleep 300' "bash $sup start" &
decoy_pid=$!
own "$decoy_pid"
decoy_id="$decoy_pid $(proc_start "$decoy_pid")"
decoy_cmd=$(tr '\0' ' ' <"/proc/$decoy_pid/cmdline" 2>/dev/null)
if [[ "$decoy_cmd" == *"$sup start"* ]]; then
  ok "fixture: a look-alike process matches the old broad pattern (pid $decoy_pid)"
else
  bad "fixture: the look-alike process does not carry the supervisor pattern ('$decoy_cmd')"
fi

crash_mid_start
if [ -n "$crash_child" ] && is_alive "$crash_child" && [ -n "$(marked "$crash_marker")" ]; then
  ok "fixture: the interrupted start left a provisional record over a live child"
  out=$(sv SLASHIT_TEST_CLI_NEVER_ANSWERS=1 start 2>&1); st=$?
  expect_refused_alive "a repeated start is refused while the provisional child is alive" "$out" "$st" "still alive but does not answer" "$crash_child"
  out=$(sv SLASHIT_TEST_CLI_NEVER_ANSWERS=1 SLASHIT_DOGFOOD_STOP_TIMEOUT=2 SLASHIT_DOGFOOD_FORCE_TIMEOUT=2 stop --force 2>&1); st=$?
  if [ "$st" -eq 0 ] && ! is_alive "$crash_child"; then
    ok "stop --force ends the child of the interrupted start"
  else
    bad "stop --force did not end the interrupted child (exit $st)"; printf '%s\n' "$out" >&2
  fi
else
  bad "crash fixture: no provisional record over a live child (child '$crash_child')"; cat "$work/crash.out" >&2 2>/dev/null
fi
cleanup_instances

crash_mid_start
if [ -n "$crash_child" ] && is_alive "$crash_child"; then
  owned_by_fixture "$crash_child" && kill -9 "$crash_child" 2>/dev/null
  for i in $(seq 50); do is_alive "$crash_child" || break; sleep 0.1; done
  wait_lock_free
  out=$(sv start 2>&1); st=$?
  new_child=$(stub_pid)
  if [ "$st" -eq 0 ] && [ -n "$new_child" ] && [ "$new_child" != "$crash_child" ] && is_alive "$new_child"; then
    ok "once the interrupted child has exited, its provisional record no longer blocks a start"
  else
    bad "start after the interrupted child exited did not recover (exit $st)"; printf '%s\n' "$out" >&2
  fi
else
  bad "recovery fixture: no live child to exit"
fi
cleanup_instances

# The look-alike survived both interrupted supervisors. A stale pid file that
# names it (its pid reused by an unrelated process) must not make cleanup signal
# it either: cleanup signals only a process that carries the fixture marker.
if is_alive "$decoy_pid"; then
  ok "the look-alike process survived both interrupted supervisors (only the fixture's own supervisor was ended)"
else
  bad "the look-alike process was killed by a fixture (pattern-based or unverified signal)"
fi
printf '%s\n' "$decoy_pid" >"$stub_dir/pid"
cleanup_instances
if is_alive "$decoy_pid" && [ ! -e "$stub_dir/pid" ]; then
  ok "cleanup does not signal a stale pid file that names an unrelated live process"
else
  bad "cleanup signaled a process that is not the fixture's instance"
fi
signal_owned KILL "$decoy_id" || true
wait "$decoy_pid" 2>/dev/null || true

cleanup_instances
rm -f "$record_file"
mkdir "$record_file"
m_persist=persist-$$
out=$(sv SLASHIT_TEST_MARK=$m_persist SLASHIT_TEST_STUB_ROLE=noquit start 2>&1); st=$?
rmdir "$record_file"
if [ "$st" -eq 1 ] && [[ "$out" == *"cannot write the provisional launch record"* ]] && [[ "$out" != *"started pid"* ]] && [ -z "$(marked "$m_persist")" ]; then
  ok "a provisional record that cannot be written refuses the start and ends its child"
else
  bad "a failed provisional record write was not a refused start with its child ended (exit $st)"; printf '%s\n' "$out" >&2
fi
if [[ "$out" == *"no launch record was written for it"* ]] && [[ "$out" != *"kept"* ]]; then
  ok "a provisional record that cannot be written is not reported as kept"
else
  bad "a failed provisional record write was reported as a kept record (exit $st)"; printf '%s\n' "$out" >&2
fi
cleanup_instances

# A start whose provisional record cannot be written (its record path is a
# directory) and whose child's /proc identity is unreadable. The child is left
# running on purpose: the case checks that nothing signals it, and it is ended
# below by the fixture's own ownership check. $1 is a PATH directory whose `cat`
# shim makes the read fail; $2 is the child's marker.
unrecorded_start() {
  cleanup_instances
  rm -f "$record_file"
  mkdir "$record_file"
  out=$(sv PATH="$1:$PATH" SLASHIT_TEST_MARK=$2 SLASHIT_TEST_STUB_ROLE=noquit start 2>&1); st=$?
  rmdir "$record_file"
  # The child writes its pid file on startup. Wait for it, then give a wrong
  # signal time to land before the child is judged.
  local i
  for i in $(seq 100); do [ -s "$stub_dir/pid" ] && break; sleep 0.05; done
  sleep 0.5
}
blind_cat=$(command -v cat)
blind_shim=$work/blind-shim
mkdir -p "$blind_shim"
cat >"$blind_shim/cat" <<SHIM
#!/usr/bin/env bash
for a in "\$@"; do
  case \$a in /proc/[0-9]*/stat) exit 1 ;; esac
done
exec "$blind_cat" "\$@"
SHIM
chmod +x "$blind_shim/cat"
m_blind=blind-$$
unrecorded_start "$blind_shim" "$m_blind"
if [ "$st" -eq 1 ] && [[ "$out" == *"has no captured identity, so nothing was signaled; no launch record was written for it"* ]] && [[ "$out" != *"kept"* ]] && [[ "$out" != *"started pid"* ]] && [ -n "$(marked "$m_blind")" ]; then
  ok "a failed provisional write with an unreadable identity claims no record, and signals nothing (refused: no captured identity)"
else
  bad "a failed provisional write with an unreadable identity was misreported or its child was signaled (exit $st)"; printf '%s\n' "$out" >&2
fi
cleanup_instances

# The same, but the identity is readable at the spawn and unreadable at the
# abort, which reaches the unknown branch with no signal sent. The shim fails
# every /proc stat read after the first one (the spawn's identity capture).
once_flag=$work/once-flag
once_shim=$work/once-shim
mkdir -p "$once_shim"
rm -f "$once_flag"
cat >"$once_shim/cat" <<SHIM
#!/usr/bin/env bash
for a in "\$@"; do
  case \$a in
    /proc/[0-9]*/stat)
      if [ -e "$once_flag" ]; then exit 1; fi
      : >"$once_flag" ;;
  esac
done
exec "$blind_cat" "\$@"
SHIM
chmod +x "$once_shim/cat"
m_once=once-$$
unrecorded_start "$once_shim" "$m_once"
if [ "$st" -eq 1 ] && [[ "$out" == *"cannot be verified, so no further signal was sent; no launch record was written for it"* ]] && [[ "$out" != *"kept"* ]] && [ -n "$(marked "$m_once")" ]; then
  ok "a failed provisional write with an unverifiable child claims no record, and sends no signal (refused: cannot be verified)"
else
  bad "a failed provisional write with an unverifiable child was misreported or its child was signaled (exit $st)"; printf '%s\n' "$out" >&2
fi
cleanup_instances

rm -f "$record_file"
m_verified=verified-$$
out=$(sv SLASHIT_TEST_MARK=$m_verified SLASHIT_TEST_STUB_ROLE=noquit SLASHIT_TEST_CLI_REPLACE_RECORD="$record_file" start 2>&1); st=$?
rm -rf "$record_file"
if [ "$st" -eq 1 ] && [[ "$out" == *"cannot write the verified launch record"* ]] && [[ "$out" != *"started pid"* ]] && [ -z "$(marked "$m_verified")" ]; then
  ok "a verified record that cannot be written is not reported as a successful start, and its child ends"
else
  bad "a failed verified record write was reported as success or left its child (exit $st)"; printf '%s\n' "$out" >&2
fi
cleanup_instances

echo "== XDG paths: trailing and repeated slashes cannot bypass the default-directory gate"
expect fail "a default runtime directory with a trailing slash is refused" \
  "$(sv SLASHIT_DOGFOOD_RUNTIME="/run/user/$(id -u)/" env 2>&1)" $? "default directory derived from HOME"
expect fail "a default runtime directory with repeated slashes is refused" \
  "$(sv SLASHIT_DOGFOOD_RUNTIME="/run/user/$(id -u)//" env 2>&1)" $? "default directory derived from HOME"
expect fail "a default state home with a trailing slash is refused" \
  "$(sv SLASHIT_DOGFOOD_STATE="$HOME/.local/state/" env 2>&1)" $? "default directory derived from HOME"
expect fail "HOME with a trailing slash as the state base is refused" \
  "$(sv SLASHIT_DOGFOOD_STATE="$HOME/" env 2>&1)" $? "isolates nothing"
out=$(sv SLASHIT_DOGFOOD_RUNTIME="$runtime/" env 2>&1); st=$?
if [ "$st" -eq 0 ] && grep -qxF "XDG_RUNTIME_DIR  $runtime" <<<"$out"; then
  ok "a legitimate isolated runtime with a trailing slash is kept, in normalized form"
else
  bad "a legitimate runtime with a trailing slash was not kept in normalized form (exit $st)"; printf '%s\n' "$out" >&2
fi

echo "== XDG paths: '.' and '..' components cannot name a default directory"
# Each of these names a default location without spelling it: the old gate
# compared spellings, so each was accepted and the instance used the real state.
expect fail "a state base spelled with a trailing /. is refused, not accepted as the default" \
  "$(sv SLASHIT_DOGFOOD_STATE="$HOME/.local/state/." env 2>&1)" $? "may not contain '.' or '..'"
expect fail "a state base with a '..' component is refused" \
  "$(sv SLASHIT_DOGFOOD_STATE="$state/../state" env 2>&1)" $? "may not contain '.' or '..'"
expect fail "a runtime directory with a /. component is refused" \
  "$(sv SLASHIT_DOGFOOD_RUNTIME="/run/user/$(id -u)/." env 2>&1)" $? "may not contain '.' or '..'"
expect fail "a HOME with a '.' component cannot shift the defaults it derives" \
  "$(sv HOME="$HOME/." SLASHIT_DOGFOOD_STATE="$HOME/.local/state" env 2>&1)" $? "may not contain '.' or '..'"

echo "== XDG paths: HOME and state trailing slashes are compared in normalized form"
expect fail "a HOME with a trailing slash still derives the defaults, so the default state is refused" \
  "$(sv HOME="$HOME/" SLASHIT_DOGFOOD_STATE="$HOME/.local/state" env 2>&1)" $? "default directory derived from HOME"
out=$(sv HOME="$HOME/" env 2>&1); st=$?
if [ "$st" -eq 0 ] && grep -qxF "STATE_BASE       $state" <<<"$out"; then
  ok "an isolated state base is still accepted when HOME has a trailing slash"
else
  bad "an isolated state base was refused under a trailing-slash HOME (exit $st)"; printf '%s\n' "$out" >&2
fi
out=$(sv SLASHIT_DOGFOOD_STATE="$state/" env 2>&1); st=$?
if [ "$st" -eq 0 ] && grep -qxF "STATE_BASE       $state" <<<"$out"; then
  ok "a trailing slash on the isolated state base is kept, in normalized form"
else
  bad "a trailing slash on the isolated state base was not normalized (exit $st)"; printf '%s\n' "$out" >&2
fi

echo
if [ "$((passed + failed))" -ne "$EXPECTED_TOTAL" ]; then
  bad "the case count drifted: $((passed + failed)) cases checked, expected $EXPECTED_TOTAL"
fi
echo "$passed passed, $failed failed ($EXPECTED_TOTAL expected)"
[ "$failed" -eq 0 ] && [ "$((passed + failed))" -eq "$EXPECTED_TOTAL" ]
