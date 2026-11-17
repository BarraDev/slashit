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
#       backup, and the rollback candidate. Changes nothing.
#
#   scripts/refresh-dogfood.sh --apply [REF]
#       Refuses when the tree has uncommitted changes, when the running instance
#       cannot be verified, or when the target is not a descendant of the running
#       commit. Builds only when the promoted build is not the target's verified
#       build, then runs `supervise-dogfood.sh restart` (no --force). Does nothing
#       when the target is already the running commit.
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
sidecar=$root/target/dogfood/slashit-ui.frontend
prev_sidecar=$root/target/dogfood/previous/slashit-ui.frontend

usage() {
  echo "usage: $SELF --check|--apply [REF]" >&2
  exit 2
}

[ "$#" -ge 1 ] && [ "$#" -le 2 ] || usage
mode=$1
ref=${2:-HEAD}
case $mode in --check | --apply) ;; *) usage ;; esac

git -C "$root" rev-parse -q --verify "$ref^{commit}" >/dev/null 2>&1 ||
  { echo "$SELF: '$ref' is not a commit in $root" >&2; exit 2; }
target=$(git -C "$root" rev-parse "$ref^{commit}")
pending=$(git -C "$root" status --porcelain 2>/dev/null | wc -l)

# Field of a key=value sidecar, empty when absent.
field() { sed -n "s/^$2=//p" "$1" 2>/dev/null | head -n 1; }

built_mode=$(field "$sidecar" mode)
built_sha=$(field "$sidecar" sha256)
built_commit=$(field "$sidecar" commit)
built_dirty=$(field "$sidecar" dirty)
actual_sha=$(sha256sum "$root/target/dogfood/slashit-ui" 2>/dev/null | cut -d' ' -f1)
built_verified=no
if [ "$built_mode" = embedded ] && [ -n "$actual_sha" ] && [ "$actual_sha" = "$built_sha" ]; then
  built_verified=yes
fi

# The running instance, from the supervisor's own status. Exit 3 is unverifiable.
st=0
status_out=$("$sup" status 2>&1) || st=$?
running=unverifiable
running_commit=
if [ "$st" -eq 3 ]; then
  running=unverifiable
elif printf '%s\n' "$status_out" | grep -q '^instance: *running'; then
  running=running
  running_commit=$(printf '%s\n' "$status_out" | awk '/^commit:/ { print $2; exit }')
elif printf '%s\n' "$status_out" | grep -q '^instance: *stopped'; then
  running=stopped
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

state_base=$("$sup" env 2>/dev/null | awk '/^STATE_BASE/ { print $2 }')
latest_backup=$(ls -1d "$state_base"/supervisor/backup/[0-9]* 2>/dev/null | tail -n 1)
prev_commit=$(field "$prev_sidecar" commit)

short() { printf '%s' "${1:-none}" | cut -c1-12; }

report() {
  echo "target commit:    $(short "$target") ($ref)"
  echo "pending changes:  $pending uncommitted path(s)"
  echo "running:          $running${running_commit:+ at $(short "$running_commit")}"
  echo "forward only:     $forward (target descends from the running commit)"
  echo "built:            $(short "$built_commit") mode=${built_mode:-none} dirty=${built_dirty:-unknown} verified=$built_verified"
  echo "latest backup:    ${latest_backup:-none}"
  echo "rollback build:   $(short "$prev_commit") (in $root/target/dogfood/previous/)"
}

if [ "$mode" = --check ]; then
  report
  [ "$running" != unverifiable ] || exit 3
  { [ "$pending" -eq 0 ] && [ "$forward" != NO ]; } || exit 1
  exit 0
fi

# --apply ---------------------------------------------------------------------
report
[ "$pending" -eq 0 ] || { echo "$SELF: refusing: uncommitted changes; commit or stash them first" >&2; exit 1; }
[ "$running" != unverifiable ] || { echo "$SELF: refusing: the running instance cannot be verified (see status)" >&2; exit 3; }
[ "$forward" != NO ] || { echo "$SELF: refusing: the target does not descend from the running commit; state is never handed to an older build" >&2; exit 1; }
if [ "$running" = running ] && [ "$running_commit" = "$target" ]; then
  echo "$SELF: the running instance already runs ${target:0:12}; nothing to do"
  exit 0
fi

if [ "$built_verified" = yes ] && [ "$built_commit" = "$target" ] && [ "$built_dirty" = no ]; then
  echo "$SELF: the promoted build is ${target:0:12}, verified; skipping the build"
else
  echo "$SELF: building ${target:0:12}"
  (cd "$root" && "$build") || { echo "$SELF: the build failed; the running instance was not touched" >&2; exit 1; }
fi

"$sup" restart || { echo "$SELF: the restart did not complete; see the supervisor's message above" >&2; exit 1; }
echo "$SELF: refreshed to ${target:0:12}"
