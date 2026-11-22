#!/usr/bin/env bash
# SPDX-License-Identifier: MPL-2.0
# Report on, or refresh, the dogfood instance from this checkout. It needs no AI
# agent and runs no agent. It composes the two existing scripts: the build
# (scripts/build-dogfood-app.sh) and the supervisor (scripts/supervise-dogfood.sh).
#
#   scripts/refresh-dogfood.sh --check [REF]
#       Read-only. Reports the target commit (REF, default HEAD), pending changes,
#       the running commit, whether the target is a descendant of the running
#       commit (state is never handed to an older build), the built commit and
#       whether the promoted binary still matches its sidecar, the latest state
#       backup, the rollback candidate, the running executable (read from /proc,
#       not the selected artifact) and the state-writer provenance. Changes
#       nothing. Exits 1 and names every "apply blocked" precondition (a missing
#       state-writer record for existing state, a legacy running executable, a
#       downgrade) when apply would refuse, so a clean report is never a promise
#       that apply will run.
#
#   scripts/refresh-dogfood.sh --apply [REF]
#       Refuses when the tree has uncommitted changes, when the running instance
#       cannot be verified, or when the target is not a descendant of the running
#       commit or of the last build that wrote the state (the supervisor's
#       state-writer file, which survives a stop). Builds only when the promoted
#       build is not the target's verified build, and only when this checkout's
#       HEAD is the target. Before starting anything it checks that the promoted
#       build's sidecar names the target commit and the binary's sha256. Then runs
#       `supervise-dogfood.sh restart` (no --force), or `start` when stopped, with
#       SLASHIT_DOGFOOD_EXPECT_COMMIT set to the target: the supervisor refuses any
#       other build before it stops or starts anything.
#       A restart of a running instance also refuses when `supervise-dogfood.sh
#       activity` reports work the restart would interrupt (terminals, Workers,
#       Queue or in-progress Tasks, a recovery copy) or cannot verify it. The check
#       runs before the build and again right before the restart. It does not close
#       admission, so a Task admitted between the second check and the stop is not
#       covered; see the PR notes.
#       Does nothing when the target is already the running commit.
#
# The artifact root (the promoted binary, its sidecar and previous/) is the one
# the supervisor reports in `env`, so a linked worktree and the main checkout
# act on the same build. Restoring an older executable never makes state safe:
# an older build must not run over state a newer one has written, and no
# backward compatibility of state is assumed.
#
# Exit status: 0 ready or done; 1 refused or failed; 2 usage; 3 unverifiable (the
# running instance could not be checked, so nothing is claimed about it).
#
# Test overrides (the defaults are the installation): SLASHIT_REFRESH_BUILD and
# SLASHIT_REFRESH_SUPERVISOR name the build and supervisor scripts to run.
set -uo pipefail
export LC_ALL=C

SELF=${0##*/}
root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
sup=${SLASHIT_REFRESH_SUPERVISOR:-$root/scripts/supervise-dogfood.sh}
build=${SLASHIT_REFRESH_BUILD:-$root/scripts/build-dogfood-app.sh}

usage() {
  echo "usage: $SELF --check|--apply [REF]" >&2
  exit 2
}

[ "$#" -ge 1 ] && [ "$#" -le 2 ] || usage
mode=$1
ref=${2:-HEAD}
case $mode in --check | --apply) ;; *) usage ;; esac

git -C "$root" rev-parse --git-dir >/dev/null 2>&1 ||
  { echo "$SELF: $root is not a Git repository (a JJ-only workspace has no .git); run this from a Git checkout" >&2; exit 2; }
git -C "$root" rev-parse -q --verify "$ref^{commit}" >/dev/null 2>&1 ||
  { echo "$SELF: '$ref' is not a commit in $root" >&2; exit 2; }
target=$(git -C "$root" rev-parse "$ref^{commit}")
pending=$(git -C "$root" status --porcelain 2>/dev/null | wc -l)

# Field of a key=value sidecar, empty when absent.
field() { sed -n "s/^$2=//p" "$1" 2>/dev/null | head -n 1; }

# The supervisor is the one authority for where the build lives and where the
# state is; refresh never derives either itself.
sup_env=$("$sup" env 2>/dev/null)
# The value is everything after the key and its padding, so a path may contain spaces.
envf() { printf '%s\n' "$sup_env" | awk -v k="$1" '$1 == k { sub(/^[^ ]+ +/, ""); print; exit }'; }
binary=$(envf BINARY)
case $binary in /*) ;; *) echo "$SELF: the supervisor cannot resolve the dogfood binary ('$binary')" >&2; exit 2 ;; esac
sidecar=$binary.frontend
prev_sidecar=$(dirname "$binary")/previous/slashit-ui.frontend
state_base=$(envf STATE_BASE)
writer_file=$(envf STATE_WRITER)
writer_commit=$(field "$writer_file" commit)

built_mode=$(field "$sidecar" mode)
built_sha=$(field "$sidecar" sha256)
built_commit=$(field "$sidecar" commit)
built_dirty=$(field "$sidecar" dirty)
actual_sha=$(sha256sum "$binary" 2>/dev/null | cut -d' ' -f1)
built_verified=no
if [ "$built_mode" = embedded ] && [ -n "$actual_sha" ] && [ "$actual_sha" = "$built_sha" ]; then
  built_verified=yes
fi

# The running instance, from the supervisor's own status. Exit 0 verifies it.
# Exit 1 is ambiguous: it is a stale launch record (the endpoint is served by
# another pid), a degraded frontend, or a verified instance whose frontend
# predates the promoted build. Only the last is a restart target, and only when
# the output proves the process, endpoint and environment. Every other status
# (3: unverifiable, 4: environment mismatch, a record mismatch) can print a
# commit, but the text is not evidence of what runs, so it is never read as the
# running commit.
st=0
status_out=$("$sup" status 2>&1) || st=$?
running=unverifiable
running_commit=
frontend_stale=no
activity_report=
if [ "$st" -eq 0 ] && grep -q '^instance: *running' <<<"$status_out"; then
  running=running
  running_commit=$(printf '%s\n' "$status_out" | awk '/^commit:/ { print $2; exit }')
elif [ "$st" -eq 0 ] && grep -q '^instance: *stopped' <<<"$status_out"; then
  running=stopped
elif [ "$st" -eq 1 ] && grep -q '^instance: *running' <<<"$status_out" &&
  grep -q '^control channel: answering on the declared endpoint' <<<"$status_out" &&
  grep -q '^environment: *verified' <<<"$status_out" &&
  grep -q '^frontend: *STALE INSTANCE' <<<"$status_out" &&
  ! grep -q '^record: *STALE' <<<"$status_out"; then
  running=running
  frontend_stale=yes
  running_commit=$(printf '%s\n' "$status_out" | awk '/^commit:/ { print $2; exit }')
fi

forward=n/a
if [ "$running" = running ] && [ -n "$running_commit" ] &&
  git -C "$root" cat-file -e "$running_commit^{commit}" 2>/dev/null; then
  if git -C "$root" merge-base --is-ancestor "$running_commit" "$target" 2>/dev/null; then
    forward=yes
  else
    forward=NO
  fi
fi

# The last build that wrote the state must be the target or its ancestor,
# whether or not an instance runs now.
state_forward=n/a
if [ -n "$writer_commit" ]; then
  if git -C "$root" merge-base --is-ancestor "$writer_commit" "$target" 2>/dev/null; then
    state_forward=yes
  else
    state_forward=NO
  fi
elif [ -e "$writer_file" ]; then
  state_forward=NO
fi
# The newest backup the supervisor marked complete after verifying it. A newer
# directory without the marker is shown apart: it is not a recovery point.
latest_backup=
latest_unmarked=
while IFS= read -r d; do
  if [ -f "$d/complete" ]; then latest_backup=$d; break; fi
  [ -n "$latest_unmarked" ] || latest_unmarked=$d
done < <(ls -1d "$state_base"/supervisor/backup/[0-9]* 2>/dev/null | sort -r)
prev_commit=$(field "$prev_sidecar" commit)

# Provenance facts for the report and the apply gate. The writer provenance comes
# from the record itself. The running executable is the file the live process
# executes (/proc), never the selected artifact. Both are reported separately.
writer_prov=missing
if [ -e "$writer_file" ]; then
  writer_prov=unmarked
  grep -qx 'provenance=verified-sidecar' "$writer_file" 2>/dev/null && writer_prov=verified-sidecar
  grep -qx 'provenance=legacy-attested' "$writer_file" 2>/dev/null && writer_prov=legacy-attested
fi
running_pid=$(printf '%s\n' "$status_out" | sed -n 's/^record: .*pid \([0-9][0-9]*\).*$/\1/p' | head -n 1)
# The running image is hashed through /proc, not read by path: a verified start
# removes its launch copy, so the link then ends in "(deleted)" while the bytes
# stay readable through /proc/PID/exe.
running_sha=
if [ "$running" = running ] && [ -n "$running_pid" ]; then
  running_sha=$(sha256sum "/proc/$running_pid/exe" 2>/dev/null | cut -d' ' -f1)
fi
writer_sha=$(field "$writer_file" sha256)
root_mismatch=no
[ -n "$running_sha" ] && [ "$running_sha" != "$actual_sha" ] && root_mismatch=yes
has_state=no
[ -n "$(find "$state_base/config/slashit-app" "$state_base/data/slashit-app" -type f -print -quit 2>/dev/null)" ] && has_state=yes

# Why apply would refuse, in the order an operator must resolve them. The pending
# check is always reported, so a dirty tree is never hidden behind a running target.
blockers=()
[ "$pending" -eq 0 ] || blockers+=("uncommitted changes in this checkout")
if [ "$running" != running ] || [ "$running_commit" != "$target" ]; then
  if [ "$has_state" = yes ] && [ "$writer_prov" = missing ]; then
    blockers+=("no state-writer record for the existing state, so start and restart refuse; establish it with scripts/supervise-dogfood.sh migrate-legacy COMMIT SHA256 (legacy executable, instance stopped) or adopt COMMIT SHA256")
  fi
  # A record with neither accepted marker is refused by the supervisor's gate, so it blocks here too.
  [ "$writer_prov" != unmarked ] ||
    blockers+=("the state-writer record names no accepted provenance, so start and restart refuse it; establish it with adopt COMMIT SHA256 or migrate-legacy COMMIT SHA256")
  [ "$state_forward" != NO ] || blockers+=("the target does not descend from the last build that wrote the state")
  [ "$forward" != NO ] || blockers+=("the target does not descend from the running commit")
  [ "$running" != running ] || [ "$forward" != n/a ] || blockers+=("the running commit (${running_commit:-none}) cannot be compared with the target")
  if [ "$running" = running ] && [ -z "$running_sha" ] && [ -n "$running_pid" ]; then
    blockers+=("the running executable of pid $running_pid cannot be read from /proc, so a failed restart could not be recovered")
  fi
  if [ "$running" = running ] && [ -n "$running_sha" ] && { [ "$writer_prov" != verified-sidecar ] || [ "$running_sha" != "$writer_sha" ]; }; then
    blockers+=("the running executable (sha256 ${running_sha:0:12}) is not the build the verified state-writer names, so a restart cannot capture it for recovery; stop the instance cooperatively, then migrate-legacy or adopt")
  fi
  # A restart would stop the instance, so work it holds must be absent.
  if [ "$running" = running ]; then
    act_rc=0; act_out=$("$sup" activity 2>&1) || act_rc=$?
    activity_report=$act_out
    case $act_rc in
      0) ;;
      1) blockers+=("a restart would interrupt the active work:${act_out#active work:}") ;;
      *) blockers+=("active work cannot be verified, so a restart is refused: $act_out") ;;
    esac
  fi
fi

short() { printf '%s' "${1:-none}" | cut -c1-12; }

report() {
  echo "target commit:    $(short "$target") ($ref)"
  echo "pending changes:  $pending uncommitted path(s)"
  echo "running:          $running${running_commit:+ at $(short "$running_commit")}"
  [ "$frontend_stale" = yes ] && echo "frontend:         stale (the verified instance predates the promoted build; a restart loads it)"
  [ -z "$activity_report" ] || echo "activity:         $activity_report"
  [ "$st" -eq 0 ] || [ "$frontend_stale" = yes ] || echo "supervisor status: exit $st; its output is not evidence of the running commit"
  echo "forward only:     $forward (target descends from the running commit)"
  echo "state writer:     $(short "$writer_commit") forward=$state_forward (target descends from the last build that wrote the state)"
  echo "artifact:         $binary"
  echo "built:            $(short "$built_commit") mode=${built_mode:-none} dirty=${built_dirty:-unknown} verified=$built_verified"
  echo "latest backup:    ${latest_backup:-none verified (no backup is marked complete)}"
  [ -z "$latest_unmarked" ] || echo "newest unmarked:  $latest_unmarked (not a verified recovery point; backups from before the marker are unmarked)"
  echo "rollback build:   $(short "$prev_commit") (in $(dirname "$binary")/previous/)"
  echo "running exe sha:  ${running_sha:-none read}${running_sha:+ (state-writer names it: $([ "$running_sha" = "$writer_sha" ] && echo yes || echo no))}"
  echo "selected artifact: $binary"
  echo "verification CLI: $(envf CLI)"
  echo "state-writer prov: $writer_prov"
  [ "$root_mismatch" = no ] || echo "note: the running executable differs from the selected artifact; a refresh replaces the running instance with the selected artifact"
  [ -n "$prev_commit" ] || echo "warning: no verified rollback build beside the selected artifact"
  if [ "${#blockers[@]}" -gt 0 ]; then printf 'apply blocked:    %s\n' "${blockers[@]}"; fi
}

if [ "$mode" = --check ]; then
  report
  [ "$running" != unverifiable ] || exit 3
  { [ "$pending" -eq 0 ] && [ "$state_forward" != NO ] && { [ "$running" != running ] || [ "$forward" = yes ]; }; } || exit 1
  # A clean, forward target is not ready while a known precondition of apply is
  # missing (the state-writer record, a legacy running executable, and so on).
  [ "${#blockers[@]}" -eq 0 ] || exit 1
  exit 0
fi

# --apply ---------------------------------------------------------------------
report
[ "$pending" -eq 0 ] || { echo "$SELF: refusing: uncommitted changes; commit or stash them first" >&2; exit 1; }
[ "$running" != unverifiable ] || { echo "$SELF: refusing: the running instance cannot be verified (see status)" >&2; exit 3; }
if [ "$running" = running ] && [ "$forward" != yes ]; then
  echo "$SELF: refusing: the target does not provably descend from the running commit (${running_commit:-no commit reported}); state is never handed to an older build" >&2
  exit 1
fi
if [ "$state_forward" = NO ]; then
  echo "$SELF: refusing: the target does not descend from ${writer_commit:-an unreadable record}, the last build that wrote the state in $state_base; an older build never runs over newer state, even when stopped" >&2
  exit 1
fi
if [ "${#blockers[@]}" -gt 0 ]; then
  printf '%s\n' "$SELF: refusing: ${blockers[0]}" >&2
  [ "${#blockers[@]}" -eq 1 ] || printf '%s\n' "$SELF: and ${#blockers[@]} more blocker(s) above; nothing was built, stopped or started" >&2
  exit 1
fi
if [ "$running" = running ] && [ "$running_commit" = "$target" ]; then
  echo "$SELF: the running instance already runs ${target:0:12}; nothing to do"
  exit 0
fi

if [ "$built_verified" = yes ] && [ "$built_commit" = "$target" ] && [ "$built_dirty" = no ]; then
  echo "$SELF: the promoted build is ${target:0:12}, verified; skipping the build"
else
  head=$(git -C "$root" rev-parse HEAD 2>/dev/null)
  if [ "$head" != "$target" ]; then
    echo "$SELF: refusing: the build runs in this checkout, whose HEAD is $(short "$head"), not the target $(short "$target"); check out the target first" >&2
    exit 1
  fi
  echo "$SELF: building ${target:0:12}"
  (cd "$root" && "$build") || { echo "$SELF: the build failed; the running instance was not touched" >&2; exit 1; }
fi

# What is about to be started must be the target's build, whatever happened above.
now_sha=$(sha256sum "$binary" 2>/dev/null | cut -d' ' -f1)
if [ "$(field "$sidecar" mode)" != embedded ] || [ -z "$now_sha" ] || [ "$now_sha" != "$(field "$sidecar" sha256)" ] ||
  [ "$(field "$sidecar" commit)" != "$target" ] || [ "$(field "$sidecar" dirty)" != no ]; then
  echo "$SELF: refusing: the promoted build at $binary is not a verified clean build of $(short "$target") (its sidecar names $(short "$(field "$sidecar" commit)")); nothing was started or restarted" >&2
  exit 1
fi

# The supervisor checks the copy it launches against this commit before it stops
# or starts anything, so a build promoted after the check above cannot be launched
# under this target's name.
# The build above can take minutes, so the active-work check runs again here, as
# close to the stop as the shell can make it. It cannot close admission (see the
# header), so it narrows the window rather than closing it.
if [ "$running" = running ]; then
  act_rc=0; act_out=$("$sup" activity 2>&1) || act_rc=$?
  if [ "$act_rc" -ne 0 ]; then
    echo "$SELF: refusing: $act_out, found immediately before the restart; nothing was stopped or restarted" >&2
    if [ "$act_rc" -eq 1 ]; then exit 1; fi
    exit 3
  fi
fi
if [ "$running" = stopped ]; then
  SLASHIT_DOGFOOD_EXPECT_COMMIT=$target "$sup" start || { echo "$SELF: the start did not complete; see the supervisor's message above" >&2; exit 1; }
else
  SLASHIT_DOGFOOD_EXPECT_COMMIT=$target "$sup" restart || { echo "$SELF: the restart did not complete; see the supervisor's message above" >&2; exit 1; }
fi
echo "$SELF: refreshed to ${target:0:12}"
