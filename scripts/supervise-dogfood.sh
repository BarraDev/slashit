#!/usr/bin/env bash
# SPDX-License-Identifier: MPL-2.0
# Launch, inspect and restart the isolated SlashIt dogfood instance.
#
# The dogfood instance is the GUI development build owned by the
# `slashit-dogfood-git` workspace. It runs detached (setsid), so until now its
# lifetime was untracked and its environment was correct only in the session
# that started it. This script makes the launch explicit and verifiable:
#
#   scripts/supervise-dogfood.sh start
#       Spawn the binary detached, under the declared environment below.
#       Refuses when an instance already answers on the control socket, and
#       when the binary does not answer --version (unknown provenance). After
#       the spawn it verifies the control channel with `slashit --endpoint
#       ... ping` and cross-checks the new PID's /proc environment against the
#       declaration.
#
#   scripts/supervise-dogfood.sh status
#       Report PID, binary path, binary-provenance commit and endpoint, and
#       whether the control channel and the environment verify. Exits 0 both
#       when the instance is running and when it is stopped.
#
#   scripts/supervise-dogfood.sh verify
#       Endpoint verification only: ping the control socket, compare the
#       answering PID and endpoint with the declaration, and verify the live
#       environment. Exits 0 only when everything checks out.
#
#   scripts/supervise-dogfood.sh stop [--force]
#       Ask the instance to shut down over the control channel (graceful, and
#       the only path that lets it end its terminals cleanly), then wait. If
#       it is still running, --force escalates: SIGTERM, then SIGKILL.
#
#   scripts/supervise-dogfood.sh restart [--force]
#       The controlled restart: stop, take a state backup under
#       <state>/supervisor/backup/ and verify the copy checksum-by-checksum,
#       start again, and prove the compared state files are unchanged
#       (checksums before and after). Exits 1 with a file list if the state
#       diverged; that is a stop-and-investigate, not a warning.
#
#   scripts/supervise-dogfood.sh env
#       Print the declared environment and the resolved paths.
#
# Declared environment (the instance may never derive any of this itself):
#
#   STATE_BASE         ~/.local/state/slashit-dogfood
#   XDG_CONFIG_HOME    $STATE_BASE/config
#   XDG_DATA_HOME      $STATE_BASE/data
#   XDG_CACHE_HOME     $STATE_BASE/cache
#   XDG_STATE_HOME     $STATE_BASE                (the base itself)
#   XDG_RUNTIME_DIR    /run/user/<uid>/slashit-dogfood
#   GH_CONFIG_DIR      ~/.config/gh   (real GitHub credentials, deliberately)
#
# HOME stays the real home, so the desktop session's display variables flow
# through unchanged. A start refuses when any declared value would equal the
# default XDG location derived from HOME (~/.config, ~/.local/share, ~/.cache,
# ~/.local/state, /run/user/<uid>), so the instance can never silently fall
# back to the owner's real state.
#
# The supervisor keeps its own files under $STATE_BASE/supervisor: the launch
# record (a key<TAB>value file), the instance log, and the restart backups.
# Those directories are excluded from every state comparison, together with
# the regenerable cache and the instance's own logs, so only real application
# state is compared across a restart.
#
# Environment overrides (tests use them; the defaults are the installation):
#
#   SLASHIT_DOGFOOD_STATE          the isolated state base
#   SLASHIT_DOGFOOD_RUNTIME        the isolated runtime base (XDG_RUNTIME_DIR)
#   SLASHIT_DOGFOOD_BINARY         the slashit-ui binary
#   SLASHIT_DOGFOOD_CLI            the `slashit` CLI used to verify the endpoint
#   SLASHIT_DOGFOOD_GH_CONFIG      GH_CONFIG_DIR override
#   SLASHIT_DOGFOOD_START_TIMEOUT  seconds to wait for the control socket
#   SLASHIT_DOGFOOD_STOP_TIMEOUT   seconds to wait for a graceful quit
#   SLASHIT_DOGFOOD_FORCE_TIMEOUT  seconds between SIGTERM and SIGKILL
#
# Exit status: 0 success, 1 refusal or failure, 2 usage error.

set -uo pipefail
export LC_ALL=C

SELF=${0##*/}

usage() {
  echo "usage: $SELF {start|stop|restart|status|verify|env} [--force]" >&2
  exit 2
}

[ "$#" -le 2 ] || usage
verb=${1:-}
[ -n "$verb" ] || usage
case ${2:-} in "" | --force) ;; *) usage ;; esac
case $verb in
  start | stop | restart | status | verify | env) ;;
  *) usage ;;
esac
case $verb in env | status) [ "${2:-}" = "" ] || usage ;; esac
case $verb in stop | restart) : ;; *) [ "${2:-}" != "--force" ] || usage ;; esac
force=0
[ "${2:-}" = "--force" ] && force=1

uid=$(id -u)
STATE_BASE=${SLASHIT_DOGFOOD_STATE:-"$HOME/.local/state/slashit-dogfood"}
RUNTIME_BASE=${SLASHIT_DOGFOOD_RUNTIME:-/run/user/$uid/slashit-dogfood}
BINARY=${SLASHIT_DOGFOOD_BINARY:-"$STATE_BASE/data/workspaces/slashit-dogfood-git/target/debug/slashit-ui"}
GH_CONFIG=${SLASHIT_DOGFOOD_GH_CONFIG:-"$HOME/.config/gh"}
START_TIMEOUT=${SLASHIT_DOGFOOD_START_TIMEOUT:-90}
STOP_TIMEOUT=${SLASHIT_DOGFOOD_STOP_TIMEOUT:-180}
FORCE_TIMEOUT=${SLASHIT_DOGFOOD_FORCE_TIMEOUT:-15}

SUP_DIR=$STATE_BASE/supervisor
RECORD=$SUP_DIR/instance
CHILD_PID=$SUP_DIR/child.pid
INSTANCE_LOG=$SUP_DIR/instance.log
SOCKET=$RUNTIME_BASE/slashit-app/slashit.sock
ENDPOINT=unix:$SOCKET

# The declared environment ---------------------------------------------------
declare_config=$STATE_BASE/config
declare_data=$STATE_BASE/data
declare_cache=$STATE_BASE/cache
declare_state=$STATE_BASE
declare_runtime=$RUNTIME_BASE

# Helpers --------------------------------------------------------------------

fail() {
  echo "$SELF: $*" >&2
  exit 1
}

note() {
  echo "$SELF: $*"
}

# A refusal explains why the requested action was not taken.
refuse() {
  echo "$SELF: refusing: $*" >&2
  exit 1
}

# Never fall back to the user's default XDG directories. The defaults are
# derived from the real HOME here, exactly the way the application and the
# shell would derive them, and any match is a refusal.
env_gates() {
  case $STATE_BASE in
    "" | "$HOME") refuse "the state base ('$STATE_BASE') isolates nothing" ;;
  esac
  case $HOME in
    "$STATE_BASE" | "$STATE_BASE"/*) refuse "HOME ($HOME) sits inside the isolated state base" ;;
  esac
  local bad=0
  [ "$declare_config" = "$HOME/.config" ] && bad=1
  [ "$declare_data" = "$HOME/.local/share" ] && bad=1
  [ "$declare_cache" = "$HOME/.cache" ] && bad=1
  [ "$declare_state" = "$HOME/.local/state" ] && bad=1
  [ "$declare_runtime" = "/run/user/$uid" ] && bad=1
  if [ "$bad" -eq 1 ]; then
    refuse "a declared XDG value equals the default directory derived from HOME ('$HOME'); the instance must never fall back to the real user state"
  fi
}

resolve_cli() {
  if [ -n "${SLASHIT_DOGFOOD_CLI:-}" ]; then
    CLI_BIN=$SLASHIT_DOGFOOD_CLI
  elif command -v slashit >/dev/null 2>&1; then
    CLI_BIN=$(command -v slashit)
  else
    CLI_BIN=
  fi
}

# `slashit ping` through the CLI, parsed without jq (serde prints one field
# per line). On success sets PING_MODE, PING_PID, PING_VERSION, PING_ENDPOINT.
ping_fields() {
  local out
  out=$("$CLI_BIN" --endpoint "$ENDPOINT" --json ping 2>/dev/null) || return 1
  PING_MODE=$(printf '%s\n' "$out" | sed -n 's/^[[:space:]]*"mode"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' | head -n 1)
  PING_PID=$(printf '%s\n' "$out" | sed -n 's/^[[:space:]]*"pid"[[:space:]]*:[[:space:]]*\([0-9][0-9]*\).*/\1/p' | head -n 1)
  PING_VERSION=$(printf '%s\n' "$out" | sed -n 's/^[[:space:]]*"version"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' | head -n 1)
  PING_ENDPOINT=$(printf '%s\n' "$out" | sed -n 's/^[[:space:]]*"endpoint"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' | head -n 1)
  [ -n "$PING_PID" ] || return 1
}

_process_alive() {
  [ -n "${1:-}" ] && kill -0 "$1" 2>/dev/null
}

record_field() {
  [ -f "$RECORD" ] || return 1
  awk -F '\t' -v key="$1" '$1 == key { print $2 }' "$RECORD" | head -n 1
}

# Provenance: the commit of the checkout the binary was built in, resolved
# through that checkout's own .git. The value is recorded at launch time.
binary_commit() {
  local dir=$1 top commit
  top=$(git -C "$dir" rev-parse --show-toplevel 2>/dev/null) || return 1
  commit=$(git -C "$top" rev-parse HEAD 2>/dev/null) || return 1
  printf '%s\n' "$commit"
}

# The new PID's real process environment, compared value-by-value against the
# declaration. This is the never-fall-back proof: the process that serves the
# socket carries exactly the declared variables, not shell-derived defaults.
ENV_CHECK=9 # 0 verified, 1 mismatch, 9 unreadable

verify_env() {
  local pid=$1 rc=0
  if [ ! -r "/proc/$pid/environ" ]; then
    ENV_CHECK=9
    return
  fi
  while IFS=' ' read -r name want; do
    [ -n "$name" ] || continue
    if ! tr '\0' '\n' <"/proc/$pid/environ" 2>/dev/null | grep -Fxq "$name=$want"; then
      rc=1
    fi
  done <<EOF
XDG_CONFIG_HOME $declare_config
XDG_DATA_HOME $declare_data
XDG_CACHE_HOME $declare_cache
XDG_STATE_HOME $declare_state
XDG_RUNTIME_DIR $declare_runtime
GH_CONFIG_DIR $GH_CONFIG
EOF
  ENV_CHECK=$rc
}

env_verdict() {
  case $ENV_CHECK in
    0) echo "environment:    verified (the live process carries the declared XDG_* and GH_CONFIG_DIR values)" ;;
    1) echo "environment:    MISMATCH — the live process does not carry the declared environment; do not trust writes from this instance, investigate /proc/<pid>/environ" ;;
    *) echo "environment:    unverifiable (the process environment could not be read)" ;;
  esac
  return "$ENV_CHECK"
}

# Which state files a restart must preserve: regular files under config/ and
# data/, relative to $STATE_BASE, excluding:
#   supervisor/ ...                       the supervisor's own records
#   cache/                                regenerable by design
#   data/slashit-app/logs/                logs are written during shutdown too
#   data/slashit-app/pr-helper-logs/      same
#   data/slashit-app/terminal_sessions.toml
#                                         rewritten at every startup (the PTY
#                                         sessions of the previous instance
#                                         are marked ended there on purpose)
# A missing base degrades to an empty comparison, which the restart check
# reports as a divergence instead of silently passing.
state_snapshot_in() {
  (
    cd "$1" 2>/dev/null || exit 0
    find config data -type f \
      ! -path './data/slashit-app/logs/*' \
      ! -path './data/slashit-app/pr-helper-logs/*' \
      ! -path './data/slashit-app/terminal_sessions.toml' \
      -print0 2>/dev/null |
      LC_ALL=C sort -z |
      xargs -0 -r sha256sum 2>/dev/null
  )
}

# Subcommands ----------------------------------------------------------------

cmd_env() {
  cat <<EOF
STATE_BASE       $STATE_BASE
XDG_CONFIG_HOME  $declare_config
XDG_DATA_HOME    $declare_data
XDG_CACHE_HOME   $declare_cache
XDG_STATE_HOME   $declare_state
XDG_RUNTIME_DIR  $declare_runtime
GH_CONFIG_DIR    $GH_CONFIG
BINARY           $BINARY
CLI              ${CLI_BIN:-<no slashit CLI found; set SLASHIT_DOGFOOD_CLI>}
SOCKET           $SOCKET
ENDPOINT         $ENDPOINT
EOF
}

cmd_start() {
  env_gates
  [ -n "$HOME" ] || fail "HOME is not set"

  [ -f "$BINARY" ] || refuse "the binary is missing: $BINARY"
  [ -x "$BINARY" ] || refuse "the binary is not executable: $BINARY"
  # The gate itself, separate from the capture: a pipe would turn the
  # binary's own exit status into the tail's status.
  "$BINARY" --version >/dev/null 2>&1 ||
    refuse "the binary does not answer --version; refusing a build with unknown provenance: $BINARY"
  version_line=$("$BINARY" --version 2>/dev/null | head -n 1)
  [ -n "$version_line" ] || refuse "the binary answers --version with nothing; refusing it: $BINARY"
  command -v setsid >/dev/null 2>&1 || refuse "setsid is required to detach the instance"
  resolve_cli
  [ -n "$CLI_BIN" ] || refuse "no \`slashit\` CLI found; set SLASHIT_DOGFOOD_CLI or put \`slashit\` on PATH, the endpoint must be verified after the launch"

  if ping_fields; then
    refuse "an instance already answers on $ENDPOINT (pid $PING_PID, mode $PING_MODE); a second instance is not permitted"
  fi
  local rec
  rec=$(record_field pid || true)
  if _process_alive "$rec"; then
    refuse "the process of the last supervised launch (pid $rec) is still alive but does not answer on $ENDPOINT; run \`$SELF status\`, then \`$SELF stop --force\`"
  fi

  mkdir -p "$STATE_BASE/config" "$STATE_BASE/data" "$STATE_BASE/cache" "$SUP_DIR" 2>/dev/null
  mkdir -p "$RUNTIME_BASE" 2>/dev/null
  chmod 700 "$STATE_BASE" "$RUNTIME_BASE" 2>/dev/null
  rm -f "$CHILD_PID"

  {
    echo "=== $(date -u +%FT%TZ) start by $SELF (user $uid)"
    printf 'XDG_CONFIG_HOME=%s\n' "$declare_config"
    printf 'XDG_DATA_HOME=%s\n' "$declare_data"
    printf 'XDG_CACHE_HOME=%s\n' "$declare_cache"
    printf 'XDG_STATE_HOME=%s\n' "$declare_state"
    printf 'XDG_RUNTIME_DIR=%s\n' "$declare_runtime"
    printf 'GH_CONFIG_DIR=%s\n' "$GH_CONFIG"
  } >>"$INSTANCE_LOG" 2>/dev/null

  (
    export \
      XDG_CONFIG_HOME="$declare_config" \
      XDG_DATA_HOME="$declare_data" \
      XDG_CACHE_HOME="$declare_cache" \
      XDG_STATE_HOME="$declare_state" \
      XDG_RUNTIME_DIR="$declare_runtime" \
      GH_CONFIG_DIR="$GH_CONFIG"
    setsid -f bash -c 'cd "$3" && printf "%s\n" "$$" > "$1" && exec "$2"' \
      supervise "$CHILD_PID" "$BINARY" "$HOME" >>"$INSTANCE_LOG" 2>&1 </dev/null
  )

  # The child records its own PID before exec'ing the binary, so the PID that
  # serves the socket and the recorded one are the same process.
  local child read_deadline=$((SECONDS + 5))
  until [ -s "$CHILD_PID" ] || [ "$SECONDS" -ge "$read_deadline" ]; do
    sleep 0.05
  done
  child=$(cat "$CHILD_PID" 2>/dev/null || true)
  rm -f "$CHILD_PID"
  [ -n "$child" ] || fail "no PID was recorded for the spawn; see $INSTANCE_LOG"

  local deadline=$((SECONDS + START_TIMEOUT))
  until ping_fields; do
    if ! _process_alive "$child"; then
      fail "the instance (pid $child) exited during startup; see $INSTANCE_LOG"
    fi
    [ "$SECONDS" -lt "$deadline" ] ||
      fail "no answer on $ENDPOINT within ${START_TIMEOUT}s; the instance (pid $child) is still alive; see $INSTANCE_LOG"
    sleep 0.25
  done

  [ "$PING_MODE" = "gui" ] || fail "the endpoint answers with mode '$PING_MODE', expected 'gui'"
  [ "${PING_ENDPOINT#unix:}" = "$SOCKET" ] ||
    fail "the instance answers on '$PING_ENDPOINT', not on '$ENDPOINT'"
  [ "$PING_PID" = "$child" ] ||
    fail "the endpoint reports pid $PING_PID, but pid $child was launched"

  verify_env "$child"
  [ "$ENV_CHECK" -eq 0 ] ||
    refuse "the spawned instance does not carry the declared environment (verdict: $(env_verdict | sed 's/^environment:    //; s/^environment: //'))"

  local commit=unknown
  commit=$(binary_commit "$(dirname "$BINARY")" 2>/dev/null) || commit=unknown

  {
    printf 'pid\t%s\n' "$child"
    printf 'binary\t%s\n' "$BINARY"
    printf 'commit\t%s\n' "$commit"
    printf 'version\t%s\n' "$version_line"
    printf 'endpoint\t%s\n' "$ENDPOINT"
    printf 'started_at\t%s\n' "$(date -u +%FT%TZ)"
  } >"$RECORD"

  note "started pid $child ($version_line, commit ${commit:0:12}) on $ENDPOINT"
  env_verdict
}

cmd_stop() {
  local pid
  pid=$(_live_pid) || fail "nothing to stop: neither the endpoint nor the last recorded launch is alive"
  if ! ping_fields; then
    note "warning: pid $pid is alive but does not answer on $ENDPOINT (a process-side hang is possible)"
  else
    if [ -z "$(record_field pid || true)" ]; then
      note "note: pid $pid was not started by this supervisor; stopping it anyway"
    fi
    if ! "$CLI_BIN" --endpoint "$ENDPOINT" quit >/dev/null 2>&1; then
      note "warning: \`slashit quit\` failed; waiting for the process to exit, then escalating"
    fi
  fi

  local deadline=$((SECONDS + STOP_TIMEOUT))
  while _process_alive "$pid" && [ "$SECONDS" -lt "$deadline" ]; do
    sleep 0.25
  done

  if _process_alive "$pid" && [ "$force" -eq 0 ]; then
    refuse "pid $pid did not stop within ${STOP_TIMEOUT}s of \`slashit quit\`; it may be waiting on a shutdown confirmation (agents or terminals still running). Rerun with --force to SIGTERM/SIGKILL it"
  fi

  if _process_alive "$pid"; then
    kill -TERM "$pid" 2>/dev/null || true
    deadline=$((SECONDS + FORCE_TIMEOUT))
    while _process_alive "$pid" && [ "$SECONDS" -lt "$deadline" ]; do
      sleep 0.25
    done
    if _process_alive "$pid"; then
      kill -KILL "$pid" 2>/dev/null || true
      sleep 0.5
      _process_alive "$pid" && fail "pid $pid survived SIGKILL; investigate manually"
    fi
    note "pid $pid ended after signal escalation; a raw SIGTERM/SIGKILL does not run the application's shutdown (terminals may outlive it)"
  else
    note "pid $pid stopped gracefully"
  fi
  rm -f "$RECORD"
}

_live_pid() {
  if ping_fields; then
    printf '%s\n' "$PING_PID"
    return 0
  fi
  local rec
  rec=$(record_field pid) || return 1
  _process_alive "$rec" && printf '%s\n' "$rec"
}

# Take the pre-start state backup and verify the copy against its manifest,
# line for line, then prune older backups so the five most recent remain.
# Prints the stamp, not the path: the caller keeps one naming rule. The $$ suffix
# keeps two restarts in the same second apart.
write_backup() {
  local stamp=$(date +%Y%m%d-%H%M%S)-$$
  local dir=$SUP_DIR/backup/$stamp
  [ -d "$STATE_BASE/config" ] || [ -d "$STATE_BASE/data" ] ||
    fail "no state directories to back up under $STATE_BASE; not backing up an empty tree"
  mkdir -p "$dir"
  cp -a "$STATE_BASE/config" "$dir/" 2>/dev/null
  cp -a "$STATE_BASE/data" "$dir/" 2>/dev/null
  local snap=$dir/manifest.sha256
  state_snapshot_in "$STATE_BASE" >"$snap"
  if ! diff -q "$snap" <(state_snapshot_in "$dir") >/dev/null 2>&1; then
    fail "the state backup at $dir does not verify against its manifest; restart abandoned and no new instance was started"
  fi
  ls -1d "$SUP_DIR"/backup/[0-9]* 2>/dev/null | head -n -5 | while read -r old; do
    rm -rf "$old"
  done
  printf '%s\n' "$stamp"
}

cmd_restart() {
  local pid
  pid=$(_live_pid) || fail "nothing to restart: neither the endpoint nor the last recorded launch is alive"
  if ! ping_fields; then
    note "warning: pid $pid does not answer on $ENDPOINT; the backup and the restart proceed from state on disk"
  fi
  note "controlled restart of pid $pid on $ENDPOINT"
  cmd_stop

  local stamp backup
  stamp=$(write_backup) || exit 1
  backup=$SUP_DIR/backup/$stamp
  note "state backed up and verified at $backup"

  cmd_start

  local after=$backup/after.sha256
  state_snapshot_in "$STATE_BASE" >"$after"
  if ! cmp -s "$after" "$backup/manifest.sha256"; then
    diff <(sort "$backup/manifest.sha256") <(sort "$after") |
      grep '^[<>]' | head -n 20 | sed 's/^/    /' >&2 || true
    fail "state files changed during the restart. The backup with the intact copy is $backup; diagnose the list above before proceeding and restore only deliberately"
  fi
  rm -f "$after"
  note "verified: state files are checksum-identical before and after the restart"
}

cmd_status() {
  local running=0
  if ping_fields; then
    running=1
  fi

  if [ "$running" -eq 1 ]; then
    echo "instance:       running (pid $PING_PID, mode $PING_MODE, version $PING_VERSION)"
    echo "endpoint:       $ENDPOINT"
    local rec
    rec=$(record_field pid || true)
    if [ -n "$rec" ]; then
      echo "binary:         $(record_field binary)"
      echo "commit:         $(record_field commit)"
      echo "record:         started $(record_field started_at), pid $rec"
      if [ "$rec" != "$PING_PID" ]; then
        echo "record:         STALE — it names pid $rec, the endpoint is served by pid $PING_PID"
      fi
    else
      echo "binary:         unknown (the running instance was not started by this supervisor)"
      echo "commit:         unknown"
      echo "record:         none"
    fi
    if [ "${PING_ENDPOINT#unix:}" = "$SOCKET" ]; then
      echo "control channel: answering on the declared endpoint"
    else
      echo "control channel: answering on '$PING_ENDPOINT', which is NOT the declared endpoint"
    fi
    verify_env "$PING_PID"
    env_verdict
  else
    echo "instance:       stopped"
    echo "endpoint:       $ENDPOINT"
    echo "binary:         (default) $BINARY"
    echo "commit:         unknown"
    local rec
    rec=$(record_field pid || true)
    if [ -n "$rec" ]; then
      if _process_alive "$rec"; then
        echo "record:         pid $rec from the last supervised start is STILL ALIVE but does not answer; run \"$SELF stop --force\" if it should be gone"
      else
        echo "record:         stale (it names pid $rec, which is gone)"
      fi
    else
      echo "record:         none"
    fi
    echo "control channel: idle"
  fi
}

cmd_verify() {
  ping_fields || fail "no instance answers on $ENDPOINT"
  [ "$PING_MODE" = "gui" ] || fail "the endpoint answers with mode '$PING_MODE', expected 'gui'"
  printf 'ping:           mode %s, pid %s, version %s, endpoint %s\n' "$PING_MODE" "$PING_PID" "$PING_VERSION" "$PING_ENDPOINT"
  [ "${PING_ENDPOINT#unix:}" = "$SOCKET" ] ||
    fail "the instance answers on '$PING_ENDPOINT', not on '$ENDPOINT'"
  echo "endpoint:       $ENDPOINT — verified"
  local rec
  rec=$(record_field pid || true)
  if [ -n "$rec" ] && [ "$rec" != "$PING_PID" ]; then
    fail "the endpoint is served by pid $PING_PID, but the launch record names pid $rec"
  fi
  verify_env "$PING_PID"
  env_verdict
  [ "$ENV_CHECK" -eq 0 ]
}

env_gates
resolve_cli

case $verb in
  env) cmd_env ;;
  start) cmd_start ;;
  stop) cmd_stop ;;
  restart) cmd_restart ;;
  status) cmd_status ;;
  verify) cmd_verify ;;
esac
