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
#       commit or of the last build that wrote the state (the supervisor's
#       state-writer file, which survives a stop). Builds only when the promoted
#       build is not the target's verified build, and only when this checkout's
#       HEAD is the target. Before starting anything it checks that the promoted
#       build's sidecar names the target commit and the binary's sha256. Then runs
#       `supervise-dogfood.sh restart` (no --force), or `start` when stopped, with
#       SLASHIT_DOGFOOD_EXPECT_COMMIT set to the target: the supervisor refuses any
#       other build before it stops or starts anything.
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

# The running instance, from the supervisor's own status. Only exit 0 verifies it.
# Any other status (1: a stale record or a degraded frontend, 3: unverifiable,
# 4: environment mismatch) can print a commit, but the text is not evidence of
# what runs, so it is never read as the running commit.
st=0
status_out=$("$sup" status 2>&1) || st=$?
running=unverifiable
running_commit=
if [ "$st" -eq 0 ] && grep -q '^instance: *running' <<<"$status_out"; then
  running=running
  running_commit=$(printf '%s\n' "$status_out" | awk '/^commit:/ { print $2; exit }')
elif [ "$st" -eq 0 ] && grep -q '^instance: *stopped' <<<"$status_out"; then
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
latest_backup=$(ls -1d "$state_base"/supervisor/backup/[0-9]* 2>/dev/null | tail -n 1)
prev_commit=$(field "$prev_sidecar" commit)

short() { printf '%s' "${1:-none}" | cut -c1-12; }

report() {
  echo "target commit:    $(short "$target") ($ref)"
  echo "pending changes:  $pending uncommitted path(s)"
  echo "running:          $running${running_commit:+ at $(short "$running_commit")}"
  [ "$st" -eq 0 ] || echo "supervisor status: exit $st; its output is not evidence of the running commit"
  echo "forward only:     $forward (target descends from the running commit)"
  echo "state writer:     $(short "$writer_commit") forward=$state_forward (target descends from the last build that wrote the state)"
  echo "artifact:         $binary"
  echo "built:            $(short "$built_commit") mode=${built_mode:-none} dirty=${built_dirty:-unknown} verified=$built_verified"
  echo "latest backup:    ${latest_backup:-none}"
  echo "rollback build:   $(short "$prev_commit") (in $(dirname "$binary")/previous/)"
}

if [ "$mode" = --check ]; then
  report
  [ "$running" != unverifiable ] || exit 3
  { [ "$pending" -eq 0 ] && [ "$state_forward" != NO ] && { [ "$running" != running ] || [ "$forward" = yes ]; }; } || exit 1
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
if [ "$running" = stopped ]; then
  SLASHIT_DOGFOOD_EXPECT_COMMIT=$target "$sup" start || { echo "$SELF: the start did not complete; see the supervisor's message above" >&2; exit 1; }
else
  SLASHIT_DOGFOOD_EXPECT_COMMIT=$target "$sup" restart || { echo "$SELF: the restart did not complete; see the supervisor's message above" >&2; exit 1; }
fi
echo "$SELF: refreshed to ${target:0:12}"
