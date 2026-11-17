#!/usr/bin/env bash
# SPDX-License-Identifier: MPL-2.0
# Coordinated version bump for a release, with checks and no tagging.
#
#   scripts/release-bump.sh --dry-run <X.Y.Z> [repo-root]
#       Read-only. Shows the current version of every file that carries it, the
#       CHANGELOG status for the target, and what --apply would change. Exits 0
#       when the bump is possible, 1 when it is refused.
#   scripts/release-bump.sh --apply <X.Y.Z> [repo-root]
#       Refuses unless every file agrees on one current version, the target is
#       different, and CHANGELOG.md has a non-empty "## [X.Y.Z]" section. Then it
#       rewrites the version in each file, refreshes Cargo.lock for the workspace
#       members (cargo metadata, no compilation), and checks that every file now
#       carries the target. If any step after the first rename fails, every file
#       already changed is restored from its backup, so a failed run leaves the
#       tree as it was. It never commits, tags, pushes or publishes; the release
#       notes are curated by a person, and scripts/release-preflight.sh remains
#       the gate before a tag exists.
#
# Files (the eleven listed in docs/releasing.md section 1, plus package-lock.json,
# whose root entries carry the version too):
#   src-tauri/tauri.conf.json (authoritative), package.json, package-lock.json,
#   Cargo.toml, src-tauri/Cargo.toml, crates/*/Cargo.toml, Cargo.lock (refreshed).
#
# Test overrides: SLASHIT_RELEASE_CARGO names the cargo to run (default: cargo);
# SLASHIT_RELEASE_FAIL_RENAME names a file whose rename is made to fail, to test
# the rollback. Exit status: 0 done or possible; 1 refused or failed; 2 usage.
set -uo pipefail
export LC_ALL=C

SELF=${0##*/}
mode=${1:-}
target=${2:-}
root=${3:-.}
cargo_bin=${SLASHIT_RELEASE_CARGO:-cargo}

usage() {
  echo "usage: $SELF --dry-run|--apply <X.Y.Z> [repo-root]" >&2
  exit 2
}
case $mode in --dry-run | --apply) ;; *) usage ;; esac
[ -n "$target" ] || usage

# The published grammar of release-preflight.sh: plain MAJOR.MINOR.PATCH.
if ! [[ $target =~ ^(0|[1-9][0-9]{0,8})\.(0|[1-9][0-9]{0,8})\.(0|[1-9][0-9]{0,8})$ ]]; then
  echo "$SELF: '$target' is not plain MAJOR.MINOR.PATCH" >&2
  exit 2
fi

cd "$root" || { echo "$SELF: cannot enter $root" >&2; exit 1; }

refuse() {
  echo "$SELF: refusing: $*" >&2
  exit 1
}

# The version declared by file $2, read as manifest $1. JSON files use the
# "version" key, TOML the top-level version key, package-lock.json its first two
# "version" entries, of which the first is the root.
read_version() { # manifest file
  case $1 in
    package-lock.json)
      awk '/"version": "/ { sub(/.*"version": "/, ""); sub(/".*/, ""); print; exit }' "$2" ;;
    *.json) sed -n 's/^ *"version": "\([^"]*\)".*/\1/p' "$2" | head -n 1 ;;
    *) sed -n 's/^version = "\([^"]*\)".*/\1/p' "$2" | head -n 1 ;;
  esac
}

files=(src-tauri/tauri.conf.json package.json package-lock.json Cargo.toml src-tauri/Cargo.toml)
for c in crates/*/Cargo.toml; do files+=("$c"); done
for f in "${files[@]}"; do [ -f "$f" ] || refuse "$f is missing"; done

declare -A current=()
for f in "${files[@]}"; do
  current[$f]=$(read_version "$f" "$f")
  [ -n "${current[$f]}" ] || refuse "cannot read the version of $f"
done
first=${current[src-tauri/tauri.conf.json]}
for f in "${files[@]}"; do
  [ "${current[$f]}" = "$first" ] ||
    refuse "$f declares ${current[$f]}, but src-tauri/tauri.conf.json declares $first; reconcile them first"
done
[ "$first" != "$target" ] || refuse "the files already declare $target"

# Changelog review: a curated section for the target must exist and be non-empty.
[ -f CHANGELOG.md ] || refuse "CHANGELOG.md is missing"
section=$(awk -v h="## [$target]" '$0 == h { on = 1; next } /^## \[/ { on = 0 } on' CHANGELOG.md)
[ -n "$(printf '%s' "$section" | tr -d '[:space:]')" ] ||
  refuse "CHANGELOG.md has no curated section for $target; write it first (the notes are not generated here)"

echo "$SELF: $first -> $target"
for f in "${files[@]}"; do
  printf '  %-36s %s -> %s\n' "$f" "${current[$f]}" "$target"
done
printf '  %-36s %s\n' "Cargo.lock" "refreshed by cargo metadata (no build)"
echo "  CHANGELOG.md                         curated section for $target present"

[ "$mode" = --dry-run ] && { echo "$SELF: dry run; nothing changed"; exit 0; }

# --apply ---------------------------------------------------------------------
# Three phases. 1: every rewritten file is written to a sibling temporary file and
# checked; nothing in the tree changes. 2: each original is backed up, then the
# temporary file is renamed over it. 3: cargo metadata refreshes Cargo.lock and
# every file is read back. A failure in phase 2 or 3 restores the backups.
bak=$(mktemp -d "${TMPDIR:-/tmp}/release-bump.XXXXXX") || refuse "cannot create a backup directory"
declare -A staged=() backup=()
renamed=()
lock_existed=no
[ -e Cargo.lock ] && lock_existed=yes

discard_staged() {
  local f
  for f in "${!staged[@]}"; do rm -f -- "${staged[$f]}"; done
  rm -rf -- "$bak"
}

# Puts every original back. Reports what was restored, and keeps the backups if
# any restore fails so they can be copied by hand.
rollback() { # reason
  local f restored=() failed=()
  for f in "${renamed[@]}"; do
    if cp -p -- "${backup[$f]}" "$f" 2>/dev/null; then restored+=("$f"); else failed+=("$f"); fi
  done
  if [ "$lock_existed" = yes ]; then
    cp -p -- "$bak/Cargo.lock" Cargo.lock 2>/dev/null || failed+=(Cargo.lock)
  else
    rm -f -- Cargo.lock
  fi
  for f in "${!staged[@]}"; do rm -f -- "${staged[$f]}"; done
  if [ ${#failed[@]} -eq 0 ]; then
    rm -rf -- "$bak"
    refuse "$1; restored ${#restored[@]} file(s), nothing left changed: ${restored[*]:-none}"
  fi
  refuse "$1; could not restore ${failed[*]}, the originals are in $bak"
}

# Phase 1: stage. A failure here has changed nothing yet.
for f in "${files[@]}"; do
  t=$(mktemp "$(dirname -- "$f")/$(basename -- "$f").bump.XXXXXX") ||
    { discard_staged; refuse "cannot create a temporary file beside $f"; }
  staged[$f]=$t
  if [ "$f" = package-lock.json ]; then
    awk -v old="$first" -v new="$target" '
      /"version": "/ {
        n++
        if (n <= 2 && index($0, "\"version\": \"" old "\"")) sub("\"version\": \"" old "\"", "\"version\": \"" new "\"")
      }
      { print }' "$f" >"$t"
  else
    awk -v old="\"$first\"" -v new="\"$target\"" '
      !done && index($0, old) && /"version": "|^version = "/ {
        sub(old, new); done = 1
      }
      { print }' "$f" >"$t"
  fi || { discard_staged; refuse "cannot stage $f"; }
  chmod --reference="$f" "$t" && [ "$(read_version "$f" "$t")" = "$target" ] ||
    { discard_staged; refuse "cannot stage $f with version $target"; }
done

# Phase 2: back up every original, then rename each staged file into place.
for i in "${!files[@]}"; do
  f=${files[$i]}
  backup[$f]="$bak/$i"
  cp -p -- "$f" "${backup[$f]}" || { discard_staged; refuse "cannot back up $f"; }
done
[ "$lock_existed" = yes ] && { cp -p -- Cargo.lock "$bak/Cargo.lock" || { discard_staged; refuse "cannot back up Cargo.lock"; }; }
for f in "${files[@]}"; do
  if [ "$f" = "${SLASHIT_RELEASE_FAIL_RENAME:-}" ] || ! mv -f -- "${staged[$f]}" "$f"; then
    rollback "cannot rename the new $f into place"
  fi
  renamed+=("$f")
  unset "staged[$f]"
done

# Phase 3: refresh Cargo.lock and read every file back.
"$cargo_bin" metadata --format-version 1 >/dev/null 2>&1 ||
  rollback "cargo metadata failed; Cargo.lock is not refreshed"
for f in "${files[@]}"; do
  got=$(read_version "$f" "$f")
  [ "$got" = "$target" ] || rollback "$f reads $got after the rewrite, expected $target"
done
"$cargo_bin" metadata --locked --format-version 1 >/dev/null 2>&1 ||
  rollback "Cargo.lock does not match the manifests after the bump"

rm -rf -- "$bak"
echo "$SELF: all ${#files[@]} files and Cargo.lock now declare $target; nothing committed or tagged"
