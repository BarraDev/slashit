#!/usr/bin/env bash
# SPDX-License-Identifier: MPL-2.0
# Launch, inspect and restart the isolated SlashIt dogfood instance.
#
# The dogfood instance is the GUI development build owned by a SlashIt
# workspace checkout. The build products live in that checkout's own
# target/debug (`slashit-ui`, the instance; `slashit`, the CLI used to
# verify the endpoint over the control channel); the workspace checkout is
# resolved from the checkout this script itself belongs to. The layout is
# therefore declared in exactly ONE place — this file. The systemd user unit
# (scripts/slashit-dogfood.service) shares it by running this script's `run`
# verb instead of naming the binary itself.
#
# The instance used to run detached (setsid) with no supervision at all, its
# lifetime untracked and its environment correct only in the session that
# started it. This script makes the launch explicit and verifiable:
#
#   scripts/supervise-dogfood.sh start
#       Spawn the binary detached, under the declared environment below. The
#       FIRST check is the control socket: a start refuses while an instance
#       already answers on it, and refuses an ambiguous socket path (present
#       but without a verified answer), before any binary handling. Then the
#       provenance gates apply to the CLI and the binary (--version must
#       answer, so nothing of unknown provenance is trusted). After the spawn
#       it verifies the control channel with `slashit --endpoint ... ping`
#       and cross-checks the new PID's /proc environment against the
#       declaration.
#
#   scripts/supervise-dogfood.sh run
#       Foreground: validate like start (environment gates, live-socket
#       refusal, binary provenance), then exec the binary with the declared
#       environment. This is the entry point the systemd user unit uses; the
#       service manager owns the process lifetime, restarts and the journal.
#
#   scripts/supervise-dogfood.sh status
#       Report PID, binary path, binary-provenance commit and endpoint, and
#       whether the control channel and the environment verify. Exits 0 both
#       when the instance is running and when it is stopped. If the endpoint
#       cannot be verified because the CLI is missing or unverified, it says
#       so truthfully and makes no claim either way.
#
#   scripts/supervise-dogfood.sh verify
#       Endpoint verification only: ping the control socket, compare the
#       answering PID and endpoint with the declaration, and verify the live
#       environment. Exits 0 only when everything checks out; reports
#       "cannot verify (no CLI)" when no verified CLI is available.
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
#       checksums before and after the restart. Exits 1 with a file list if
#       the state diverged; that is a stop-and-investigate, not a warning.
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
# record (a key<TAB>value file), the instance log, the lifecycle lock, and the
# start/run/stop/restart verbs take an advisory flock on that directory, so
# two lifecycle operations can never interleave.
#
# Which state files a restart must preserve, and therefore backs up, is the
# compared snapshot: regular files under config/ and data/, excluding the
# supervisor's own records, the regenerable cache, the instance's logs, the
# terminal session records (rewritten at every startup by design), and
# data/slashit-app/worktrees/ — the workspace and Task Checkouts under the
# instance's data directory. Uncommitted checkout content is not a restart
# invariant; the checkouts survive a restart untouched, and comparing or
# backing them up would checksum whole target/ build trees.
#
# Environment overrides (tests use them; the defaults are the installation):
#
#   SLASHIT_DOGFOOD_STATE          the isolated state base
#   SLASHIT_DOGFOOD_RUNTIME        the isolated runtime base (XDG_RUNTIME_DIR)
#   SLASHIT_DOGFOOD_BINARY         the slashit-ui binary
#   SLASHIT_DOGFOOD_CLI            the `slashit` CLI used to verify the endpoint
#                                   (may be set empty to say "no CLI"; when
#                                   unset, the CLI beside the binary is used)
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
  echo "usage: $SELF {start|run|stop|restart|status|verify|env} [--force]" >&2
  exit 2
}

[ "$#" -le 2 ] || usage
verb=${1:-}
[ -n "$verb" ] || usage
case ${2:-} in "" | --force) ;; *) usage ;; esac
case $verb in
  start | run | stop | restart | status | verify | env) ;;
  *) usage ;;
esac
case $verb in env | status | run) [ "${2:-}" = "" ] || usage ;; esac
case $verb in stop | restart) : ;; *) [ "${2:-}" != "--force" ] || usage ;; esac
force=0
[ "${2:-}" = "--force" ] && force=1

uid=$(id -u)
STATE_BASE=${SLASHIT_DOGFOOD_STATE:-"$HOME/.local/state/slashit-dogfood"}
RUNTIME_BASE=${SLASHIT_DOGFOOD_RUNTIME:-/run/user/$uid/slashit-dogfood}
GH_CONFIG=${SLASHIT_DOGFOOD_GH_CONFIG:-"$HOME/.config/gh"}
START_TIMEOUT=${SLASHIT_DOGFOOD_START_TIMEOUT:-90}
STOP_TIMEOUT=${SLASHIT_DOGFOOD_STOP_TIMEOUT:-180}
FORCE_TIMEOUT=${SLASHIT_DOGFOOD_FORCE_TIMEOUT:-15}

# One declared location ------------------------------------------------------
# The workspace checkout this script belongs to holds the dogfood binary and
# the CLI in its own target/debug. Resolving it from the script's location
# (through that checkout's git, or the checkout itself as a fallback) keeps
# the system in a single place even if the checkout's geometry changes.
script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)

repo_checkout() {
  local common top
  common=$(git -C "$script_dir" --path-format=absolute rev-parse --git-common-dir 2>/dev/null) || common=
  if [ -n "$common" ] && [ "$(basename "$common")" = .git ]; then
    dirname "$common"
    return 0
  fi
  # Not a linked worktree and not a plain clone layout: the checkout is
  # whatever toplevel the script's directory belongs to, if any.
  top=$(git -C "$script_dir" --path-format=absolute rev-parse --show-toplevel 2>/dev/null) || return 1
  printf '%s\n' "$top"
}

REPO_ROOT=$(repo_checkout) || REPO_ROOT=
BINARY=
if [ -n "${SLASHIT_DOGFOOD_BINARY:-}" ]; then
  BINARY=$SLASHIT_DOGFOOD_BINARY
elif [ -n "$REPO_ROOT" ]; then
  BINARY=$REPO_ROOT/target/debug/slashit-ui
fi

SUP_DIR=$STATE_BASE/supervisor
RECORD=$SUP_DIR/instance
CHILD_PID=$SUP_DIR/child.pid
INSTANCE_LOG=$SUP_DIR/instance.log
LOCK=$SUP_DIR/lifecycle.lock
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

# The CLI that verifies the endpoint is the CLI beside the binary (the same
# target/debug of the workspace checkout) or SLASHIT_DOGFOOD_CLI — an
# unverified `slashit` found on PATH must never be trusted here, so PATH is
# never consulted. Setting SLASHIT_DOGFOOD_CLI empty says "no CLI".
resolve_cli() {
  CLI_BIN=
  if [ "${SLASHIT_DOGFOOD_CLI+set}" = set ]; then
    CLI_BIN=$SLASHIT_DOGFOOD_CLI
  elif [ -n "$BINARY" ]; then
    local dir
    dir=$(dirname "$BINARY")
    if [ -f "$dir/slashit" ] && [ -x "$dir/slashit" ]; then
      CLI_BIN=$dir/slashit
    fi
  fi
}

# CLI_STATE: no | broken | ready. Every endpoint claim distinguishes these:
# without a verified CLI there is nothing to answer for.
cli_state() {
  CLI_STATE=no
  [ -n "$CLI_BIN" ] || return 0
  if "$CLI_BIN" --version >/dev/null 2>&1; then
    CLI_STATE=ready
  else
    CLI_STATE=broken
  fi
}

cannot_verify_text() {
  case $CLI_STATE in
    broken) echo "cannot verify (the CLI does not answer --version: $CLI_BIN)" ;;
    *) echo "cannot verify (no CLI)" ;;
  esac
}

# What the endpoint check can honestly say right now, for messages about a
# socket path whose answer is unverified.
verification_state_text() {
  if [ "$CLI_STATE" = ready ]; then
    echo "the CLI is verified but no instance answered on it"
  else
    cannot_verify_text
  fi
}

# `slashit ping --json` prints the InstanceInfo reply as serde pretty JSON:
# one '"key": value' pair per line, string values quoted, numbers bare, and
# every field of that struct on its own line. Parsing reads exactly that
# shape in plain substitution — no jq dependency, no regex spanning the
# document. A CLI whose ping shape ever drifts is caught at once: the parsed
# PID goes missing and every caller treats that as "no verified answer".
ping_json_field() { # key, JSON on stdin
  local key=$1 token line v
  token="\"$key\":"
  while IFS= read -r line; do
    case $line in *"$token"*) ;; *) continue ;; esac
    v=${line#*"$token"}
    v=${v%,} # the trailing comma every field but the last carries
    v=${v//\"/}
    v=${v#"${v%%[![:space:]]*}"}
    printf '%s\n' "$v"
    return 0
  done
  return 1
}

# On success sets PING_MODE, PING_PID, PING_VERSION, PING_ENDPOINT.
ping_fields() {
  # A CLI that failed its provenance gate must not become trusted merely
  # because a later invocation happens to print a plausible reply.
  [ "$CLI_STATE" = ready ] || return 1
  [ -n "$CLI_BIN" ] || return 1
  local out
  out=$("$CLI_BIN" --endpoint "$ENDPOINT" --json ping 2>/dev/null) || return 1
  PING_MODE=$(printf '%s\n' "$out" | ping_json_field mode) || return 1
  PING_PID=$(printf '%s\n' "$out" | ping_json_field pid) || return 1
  PING_VERSION=$(printf '%s\n' "$out" | ping_json_field version) || return 1
  PING_ENDPOINT=$(printf '%s\n' "$out" | ping_json_field endpoint) || return 1
  case $PING_PID in '' | *[!0-9]*) return 1 ;; esac
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
# data/ of $STATE_BASE, excluding
#   data/slashit-app/logs/                  logs are written during shutdown too
#   data/slashit-app/pr-helper-logs/        same
#   data/slashit-app/terminal_sessions.toml
#                                           rewritten at every startup (the PTY
#                                           sessions of the previous instance
#                                           are marked ended there on purpose)
#   data/slashit-app/worktrees/
#                                           workspace and Task Checkouts: they
#                                           hold uncommitted content, which is
#                                           not a restart invariant, and whole
#                                           target/ build trees, which are
#                                           regenerable; they are not compared
#                                           and not backed up
# The backup copies exactly this set, so a restore means copying the files the
# manifest lists. A missing base degrades to an empty comparison, which the
# restart check reports as a divergence instead of silently passing.
state_file_list() {
  (
    cd "$1" 2>/dev/null || exit 0
    find config data -type f \
      ! -path 'data/slashit-app/logs/*' \
      ! -path 'data/slashit-app/pr-helper-logs/*' \
      ! -path 'data/slashit-app/terminal_sessions.toml' \
      ! -path 'data/slashit-app/worktrees/*' \
      -print0 2>/dev/null
  )
}

# Paths are relative to the snapshotted base, so a state tree and its backup
# copy produce comparable lines; the checksums must run from inside the base.
state_snapshot() {
  (
    cd "$1" 2>/dev/null || exit 0
    state_file_list "$1" | LC_ALL=C sort -z | xargs -0 -r sha256sum 2>/dev/null
  )
}

# Advisory lock on the supervisor's own directory: start, run, stop and
# restart may not run concurrently, whoever triggers them. The lock lives for
# the duration of the operation; the spawned instance closes it so its own
# lifetime never holds the supervisor's lock.
lifecycle_lock() {
  command -v flock >/dev/null 2>&1 ||
    fail "flock (util-linux) is required to keep lifecycle operations from running concurrently"
  mkdir -p "$SUP_DIR" 2>/dev/null
  { exec 9>"$LOCK"; } 2>/dev/null || fail "cannot open the lifecycle lock $LOCK"
  flock -n 9 || refuse "another lifecycle operation (start, run, stop or restart) is already running on $SUP_DIR"
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
BINARY           ${BINARY:-"unresolved (no workspace checkout resolved; set SLASHIT_DOGFOOD_BINARY)"}
CLI              ${CLI_BIN:-"<no slashit CLI beside the binary; set SLASHIT_DOGFOOD_CLI>"}
SOCKET           $SOCKET
ENDPOINT         $ENDPOINT
EOF
}

cmd_start() {
  env_gates
  [ -n "$HOME" ] || fail "HOME is not set"

  # The FIRST check is the control socket, before any binary resolution or
  # startup handling: a launch that must not happen is not gated on the
  # layout of the thing it would launch. A socket path with no verified
  # answer is ambiguous (a stale file, or an instance its CLI cannot verify)
  # and is refused, not raced against.
  if ping_fields; then
    refuse "an instance already answers on $ENDPOINT (pid $PING_PID, mode $PING_MODE); a second instance is not permitted"
  elif [ -e "$SOCKET" ]; then
    refuse "the endpoint path $SOCKET exists but nothing could be verified on it ($(verification_state_text)); the socket file may be stale or the endpoint may be alive without a verified CLI — run \`$SELF status\` and resolve that before starting anything"
  fi
  local rec
  rec=$(record_field pid || true)
  if _process_alive "$rec"; then
    refuse "the process of the last supervised launch (pid $rec) is still alive but does not answer on $ENDPOINT; run \`$SELF status\`, then \`$SELF stop --force\`"
  fi

  # Provenance gates. The CLI first: it is what will verify the instance, so
  # an unverified slashit is never trusted for the ping. Then the binary.
  [ -n "$CLI_BIN" ] ||
    refuse "cannot verify the endpoint: no \`slashit\` CLI is available (it normally sits beside the binary; set SLASHIT_DOGFOOD_CLI) — a start refuses an unverified control channel"
  if [ "$CLI_STATE" != ready ]; then
    refuse "$(cannot_verify_text) — a start refuses an unverified CLI; resolve it beside the binary or through SLASHIT_DOGFOOD_CLI"
  fi
  [ -n "$BINARY" ] ||
    refuse "the binary location is unresolved: no workspace checkout was found next to this script (set SLASHIT_DOGFOOD_BINARY to point at slashit-ui)"
  [ -f "$BINARY" ] || refuse "the binary is missing: $BINARY"
  [ -x "$BINARY" ] || refuse "the binary is not executable: $BINARY"
  # The gate itself, separate from the capture: a pipe would turn the
  # binary's own exit status into the tail's status.
  "$BINARY" --version >/dev/null 2>&1 ||
    refuse "the binary does not answer --version; refusing a build with unknown provenance: $BINARY"
  version_line=$("$BINARY" --version 2>/dev/null | head -n 1)
  [ -n "$version_line" ] || refuse "the binary answers --version with nothing; refusing it: $BINARY"
  command -v setsid >/dev/null 2>&1 || refuse "setsid is required to detach the instance"

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
      supervise "$CHILD_PID" "$BINARY" "$HOME" >>"$INSTANCE_LOG" 2>&1 </dev/null 9>&-
  )
  # fd 9 is closed on purpose: the detached instance must not outlast the
  # supervisor while still holding its lifecycle lock.

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

cmd_run() {
  # The entry point of scripts/slashit-dogfood.service. Validation matches
  # start: environment gates, live-socket FIRST, binary provenance. Then the
  # binary is exec'd in the foreground with the declared environment, so
  # systemd owns the PID, restarts and the journal.
  env_gates
  [ -n "$HOME" ] || fail "HOME is not set"

  # The service manager's own environment (its Environment= entries) must
  # agree with this declaration; a disagreement is loud, not implicit.
  local name want val
  while IFS=' ' read -r name want; do
    [ -n "$name" ] || continue
    val=${!name:-}
    if [ -n "$val" ] && [ "$val" != "$want" ]; then
      refuse "ambient $name ('$val') disagrees with the declared value ('$want'); the unit's Environment= entries and this script must share the declaration (the run verb is the unit's entry point; a manual detached launch is \`$SELF start\`)"
    fi
  done <<EOF
XDG_CONFIG_HOME $declare_config
XDG_DATA_HOME $declare_data
XDG_CACHE_HOME $declare_cache
XDG_STATE_HOME $declare_state
XDG_RUNTIME_DIR $declare_runtime
GH_CONFIG_DIR $GH_CONFIG
EOF

  # Same FIRST check as start: refuse a live or ambiguous endpoint before
  # any binary handling. The unit then fails visibly instead of doubling an
  # instance the service manager does not know about.
  if ping_fields; then
    refuse "an instance already answers on $ENDPOINT (pid $PING_PID, mode $PING_MODE); a second instance is not permitted"
  elif [ -e "$SOCKET" ]; then
    refuse "the endpoint path $SOCKET exists but nothing could be verified on it ($(verification_state_text)); resolve that (see \`$SELF status\`) before the unit may start"
  fi
  local rec
  rec=$(record_field pid || true)
  if _process_alive "$rec"; then
    refuse "the process of the last supervised launch (pid $rec) is still alive but does not answer on $ENDPOINT; resolve it (see \`$SELF status\`) before the unit may start"
  fi

  [ -n "$BINARY" ] ||
    refuse "the binary location is unresolved: no workspace checkout was found next to this script (set SLASHIT_DOGFOOD_BINARY in the ExecStart environment)"
  [ -f "$BINARY" ] || refuse "the binary is missing: $BINARY"
  [ -x "$BINARY" ] || refuse "the binary is not executable: $BINARY"
  "$BINARY" --version >/dev/null 2>&1 ||
    refuse "the binary does not answer --version; refusing a build with unknown provenance: $BINARY"
  version_line=$("$BINARY" --version 2>/dev/null | head -n 1)
  [ -n "$version_line" ] || refuse "the binary answers --version with nothing; refusing it: $BINARY"

  mkdir -p "$STATE_BASE/config" "$STATE_BASE/data" "$STATE_BASE/cache" "$SUP_DIR" "$RUNTIME_BASE" 2>/dev/null
  chmod 700 "$STATE_BASE" "$RUNTIME_BASE" 2>/dev/null

  local commit=unknown
  commit=$(binary_commit "$(dirname "$BINARY")" 2>/dev/null) || commit=unknown

  {
    echo "=== $(date -u +%FT%TZ) run by $SELF for a service manager (user $uid)"
    printf 'XDG_CONFIG_HOME=%s\n' "$declare_config"
    printf 'XDG_DATA_HOME=%s\n' "$declare_data"
    printf 'XDG_CACHE_HOME=%s\n' "$declare_cache"
    printf 'XDG_STATE_HOME=%s\n' "$declare_state"
    printf 'XDG_RUNTIME_DIR=%s\n' "$declare_runtime"
    printf 'GH_CONFIG_DIR=%s\n' "$GH_CONFIG"
  } >>"$INSTANCE_LOG" 2>/dev/null

  # Same PID serves the socket after exec, so the record stays meaningful.
  {
    printf 'pid\t%s\n' "$$"
    printf 'binary\t%s\n' "$BINARY"
    printf 'commit\t%s\n' "$commit"
    printf 'version\t%s\n' "$version_line"
    printf 'endpoint\t%s\n' "$ENDPOINT"
    printf 'started_at\t%s\n' "$(date -u +%FT%TZ)"
  } >"$RECORD"

  note "exec $BINARY ($version_line, commit ${commit:0:12}) in the foreground as pid $$; the service manager owns this process"
  cd "$HOME" 2>/dev/null
  export \
    XDG_CONFIG_HOME="$declare_config" \
    XDG_DATA_HOME="$declare_data" \
    XDG_CACHE_HOME="$declare_cache" \
    XDG_STATE_HOME="$declare_state" \
    XDG_RUNTIME_DIR="$declare_runtime" \
    GH_CONFIG_DIR="$GH_CONFIG"
  exec 9>&- # release the lifecycle lock before the exec
  exec "$BINARY"
}

cmd_stop() {
  local pid
  pid=$(_live_pid)
  if [ -z "$pid" ]; then
    fail "$(nothing_reachable_text) — nothing to stop"
  fi
  if ping_fields; then
    if [ -z "$(record_field pid || true)" ]; then
      note "note: pid $pid was not started by this supervisor; stopping it anyway"
    fi
    if ! "$CLI_BIN" --endpoint "$ENDPOINT" quit >/dev/null 2>&1; then
      note "warning: \`slashit quit\` failed; waiting for the process to exit, then escalating"
    fi
  elif [ "$CLI_STATE" != ready ]; then
    refuse "cannot stop gracefully (no CLI): quitting over the control channel is the only stop that lets the instance end its terminals cleanly — resolve the CLI (beside the binary or through SLASHIT_DOGFOOD_CLI) and rerun, or rerun with --force to escalate to signals right away"
  else
    note "warning: pid $pid is alive but does not answer on $ENDPOINT (a process-side hang is possible)"
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

nothing_reachable_text() {
  local msg="nothing found to operate on: no instance answers on $ENDPOINT and the last supervised launch is not alive"
  [ -e "$SOCKET" ] && msg="$msg (the endpoint path $SOCKET exists but is unverified: investigate it first)"
  printf '%s\n' "$msg"
}

# Take the pre-start state backup: copy exactly the compared snapshot set,
# then verify the copy against its manifest, line for line. Prints the stamp,
# not the path: the caller keeps one naming rule. The $$ suffix keeps two
# restarts in the same second apart.
write_backup() {
  local stamp=$(date +%Y%m%d-%H%M%S)-$$
  local dir=$SUP_DIR/backup/$stamp
  [ -d "$STATE_BASE/config" ] || [ -d "$STATE_BASE/data" ] ||
    fail "no state directories to back up under $STATE_BASE; not backing up an empty tree"
  mkdir -p "$dir"
  (
    cd "$STATE_BASE" 2>/dev/null || exit 0
    state_file_list "$STATE_BASE" | while IFS= read -r -d '' rel; do
      mkdir -p "$dir/$(dirname "$rel")"
      cp -p -- "$STATE_BASE/$rel" "$dir/$rel"
    done
  )
  local snap=$dir/manifest.sha256
  state_snapshot "$STATE_BASE" >"$snap"
  if ! diff -q "$snap" <(state_snapshot "$dir") >/dev/null 2>&1; then
    fail "the state backup at $dir does not verify against its manifest; restart abandoned and no new instance was started"
  fi
  ls -1d "$SUP_DIR"/backup/[0-9]* 2>/dev/null | head -n -5 | while read -r old; do
    rm -rf "$old"
  done
  printf '%s\n' "$stamp"
}

cmd_restart() {
  local pid
  pid=$(_live_pid)
  if [ -z "$pid" ]; then
    fail "$(nothing_reachable_text) — nothing to restart"
  fi
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
  state_snapshot "$STATE_BASE" >"$after"
  if ! cmp -s "$after" "$backup/manifest.sha256"; then
    diff <(sort "$backup/manifest.sha256") <(sort "$after") |
      grep '^[<>]' | head -n 20 | sed 's/^/    /' >&2 || true
    fail "state files changed during the restart. The backup with the intact copy is $backup; diagnose the list above before proceeding and restore only deliberately"
  fi
  rm -f "$after"
  note "verified: state files are checksum-identical before and after the restart"
}

# The record part of the non-running status branch. mode: stopped or
# cannot-verify — the text stays truthful about what we could check.
_status_record_lines() {
  local rec=$2 mode=$1
  if [ -n "$rec" ]; then
    if _process_alive "$rec"; then
      if [ "$mode" = cannot-verify ]; then
        echo "record:         pid $rec from the last supervised start is STILL ALIVE; whether it answers on the endpoint is not verifiable (no CLI)"
      else
        echo "record:         pid $rec from the last supervised start is STILL ALIVE but does not answer; run \"$SELF stop --force\" if it should be gone"
      fi
    else
      echo "record:         stale (it names pid $rec, which is gone)"
    fi
  else
    echo "record:         none"
  fi
}

cmd_status() {
  local rec
  rec=$(record_field pid || true)

  if ping_fields; then
    echo "instance:       running (pid $PING_PID, mode $PING_MODE, version $PING_VERSION)"
    echo "endpoint:       $ENDPOINT"
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
    if [ "$CLI_STATE" != ready ]; then
      echo "instance:       $(cannot_verify_text) — the endpoint was not checked, so nothing is claimed about it either way"
      _status_record_lines cannot-verify "$rec"
      echo "control channel: $(cannot_verify_text)"
    else
      echo "instance:       stopped"
      _status_record_lines stopped "$rec"
      if [ -e "$SOCKET" ]; then
        echo "control channel: the endpoint path $SOCKET exists but nothing answered on it — possibly a stale socket file"
      else
        echo "control channel: idle"
      fi
    fi
    echo "endpoint:       $ENDPOINT"
    echo "binary:         ${BINARY:-"unresolved (set SLASHIT_DOGFOOD_BINARY)"}"
    echo "commit:         unknown"
  fi
}

cmd_verify() {
  case $CLI_STATE in
    no) fail "cannot verify (no CLI) — set SLASHIT_DOGFOOD_CLI (or keep the CLI beside the binary) to check the endpoint; without it nothing is claimed here" ;;
    broken) fail "cannot verify — the CLI does not answer --version ($CLI_BIN); an unverified slashit is not trusted for the endpoint" ;;
  esac
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
cli_state

case $verb in
  start | run | stop | restart) lifecycle_lock ;;
esac

case $verb in
  env) cmd_env ;;
  start) cmd_start ;;
  run) cmd_run ;;
  stop) cmd_stop ;;
  restart) cmd_restart ;;
  status) cmd_status ;;
  verify) cmd_verify ;;
esac
