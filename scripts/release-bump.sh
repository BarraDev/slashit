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
#       carries the target. If any step after the first rename fails, or SIGINT,
#       SIGTERM or SIGHUP arrives, every file is restored from the journal, so a
#       failed run leaves the tree as it was. It never commits, tags, pushes or
#       publishes; the release notes are curated by a person, and
#       scripts/release-preflight.sh remains the gate before a tag exists.
#   scripts/release-bump.sh --recover [repo-root]
#       Crash recovery. Before the first rename, --apply writes the journal
#       .release-bump-journal/ in the repository root (a copy of every original
#       file and a manifest). A run killed without a chance to clean up (SIGKILL,
#       power loss) leaves it. While it exists every mode refuses to run; --recover
#       restores every file from it, deletes a Cargo.lock that did not exist, and
#       then removes the journal. A successful or rolled-back run removes it itself.
#
# Files (the eleven listed in docs/releasing.md section 1, plus package-lock.json,
# whose two root entries, "version" and packages[""].version, carry it too):
#   src-tauri/tauri.conf.json (authoritative), package.json, package-lock.json,
#   Cargo.toml, src-tauri/Cargo.toml, crates/*/Cargo.toml, Cargo.lock (refreshed).
#
# Test override: SLASHIT_RELEASE_CARGO names the cargo to run (default: cargo), so
# one test can pick a different stub per run. Exit status: 0 done, possible or
# recovered; 1 refused or failed; 2 usage; 128+N when stopped by signal N.
set -uo pipefail
export LC_ALL=C

SELF=${0##*/}
mode=${1:-}
cargo_bin=${SLASHIT_RELEASE_CARGO:-cargo}
journal=.release-bump-journal

usage() {
  echo "usage: $SELF --dry-run|--apply <X.Y.Z> [repo-root] | $SELF --recover [repo-root]" >&2
  exit 2
}
case $mode in
  --recover) target=""; root=${2:-.} ;;
  --dry-run | --apply) target=${2:-}; root=${3:-.}; [ -n "$target" ] || usage ;;
  *) usage ;;
esac

# The published grammar of release-preflight.sh: plain MAJOR.MINOR.PATCH.
if [ "$mode" != --recover ] && ! [[ $target =~ ^(0|[1-9][0-9]{0,8})\.(0|[1-9][0-9]{0,8})\.(0|[1-9][0-9]{0,8})$ ]]; then
  echo "$SELF: '$target' is not plain MAJOR.MINOR.PATCH" >&2
  exit 2
fi

cd "$root" || { echo "$SELF: cannot enter $root" >&2; exit 1; }

refuse() {
  echo "$SELF: refusing: $*" >&2
  exit 1
}

# package-lock.json carries the version twice: the top-level "version" and
# packages[""].version. Prints them, in that order, one per line.
lock_versions() { # file
  awk '
    function val(s) { sub(/.*"version": "/, "", s); sub(/".*/, "", s); return s }
    /^  "version": "/ && !o { print val($0); o = 1 }
    /^    "": \{/ { r = 1; next }
    r && /^      "version": "/ { print val($0); exit }
    r && /^    \}/ { exit }' "$1"
}
lock_agrees() { # file expected: both entries carry exactly that version
  local lv
  mapfile -t lv < <(lock_versions "$1")
  [ "${#lv[@]}" -eq 2 ] && [ "${lv[0]}" = "$2" ] && [ "${lv[1]}" = "$2" ]
}

# The version declared by file $2, read as manifest $1. JSON files use the
# "version" key, TOML the top-level version key.
read_version() { # manifest file
  case $1 in
    package-lock.json) lock_versions "$2" | sed -n 1p ;;
    *.json) sed -n 's/^ *"version": "\([^"]*\)".*/\1/p' "$2" | head -n 1 ;;
    *) sed -n 's/^version = "\([^"]*\)".*/\1/p' "$2" | head -n 1 ;;
  esac
}

# Removes the journal in two steps so a kill cannot leave a manifest-less or
# partly deleted journal: the rename is the commit point, the delete is cleanup.
commit_journal() {
  mv -- "$journal" "$journal.done" || { echo "$SELF: warning: could not retire $journal; remove it by hand" >&2; exit 1; }
  rm -rf -- "$journal.done"
}

# Puts back every file listed in the journal manifest (also removing any staged
# file left beside it). Succeeds only if every file was restored.
restore_all() {
  local rc=0 b p
  while IFS=$'\t' read -r b p; do
    [ -n "$p" ] || continue
    if [ "$b" = - ]; then rm -f -- "$p" || rc=1; else cp -p -- "$journal/$b" "$p" || rc=1; fi
    rm -f -- "$p".bump.*
  done <"$journal/manifest"
  return $rc
}

if [ "$mode" = --recover ]; then
  [ -f "$journal/manifest" ] || refuse "no $journal/manifest here; nothing to recover"
  restore_all || refuse "could not restore every file; the originals remain in $journal"
  commit_journal
  echo "$SELF: restored every file from $journal and removed it"
  exit 0
fi
[ ! -e "$journal.done" ] || rm -rf -- "$journal.done"
[ ! -e "$journal" ] ||
  refuse "$journal exists: an earlier run did not finish. Run $SELF --recover to restore the original files"

files=(src-tauri/tauri.conf.json package.json package-lock.json Cargo.toml src-tauri/Cargo.toml)
for c in crates/*/Cargo.toml; do files+=("$c"); done
for f in "${files[@]}"; do [ -f "$f" ] || refuse "$f is missing"; done

declare -A current=()
for f in "${files[@]}"; do
  current[$f]=$(read_version "$f" "$f")
  [ -n "${current[$f]}" ] || refuse "cannot read the version of $f"
done
mapfile -t lv < <(lock_versions package-lock.json)
[ "${#lv[@]}" -eq 2 ] || refuse "cannot read both versions of package-lock.json"
[ "${lv[0]}" = "${lv[1]}" ] ||
  refuse "package-lock.json declares ${lv[0]} but its packages[\"\"] entry declares ${lv[1]}; reconcile them first"
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
# checked; nothing in the tree changes. 2: the journal is written (a copy of every
# original, then renamed into place whole), and each staged file is renamed over
# its original. 3: cargo metadata refreshes Cargo.lock and every file is read back.
# A failure or signal in phase 2 or 3 restores everything from the journal.
# shortcut: the journal is not fsynced, so it covers a killed process, not a power
# cut; add sync if that ever matters.
declare -A staged=()
cargo_pid=""
lock_existed=no
[ -e Cargo.lock ] && lock_existed=yes

discard_staged() {
  local f
  for f in "${!staged[@]}"; do rm -f -- "${staged[$f]}"; done
  rm -rf -- "$journal.tmp"
}

# Restores from the journal and removes it, or keeps it for --recover.
undo() { restore_all && commit_journal; }

# A signal runs the same recovery as a failure, then exits 128+N.
on_signal() { # status name
  trap '' INT TERM HUP
  if [ -n "$cargo_pid" ]; then kill "$cargo_pid" 2>/dev/null; wait "$cargo_pid" 2>/dev/null; fi
  if [ ! -d "$journal" ]; then
    discard_staged
    echo "$SELF: stopped by $2 before any file was changed" >&2
  elif undo; then
    echo "$SELF: stopped by $2; every file restored, nothing left changed" >&2
  else
    echo "$SELF: stopped by $2; could not restore every file, run $SELF --recover" >&2
  fi
  exit "$1"
}
trap 'on_signal 130 SIGINT' INT
trap 'on_signal 143 SIGTERM' TERM
trap 'on_signal 129 SIGHUP' HUP

# Runs cargo as a child the shell can be interrupted over (a foreground child would
# delay the trap until it exits).
run_cargo() {
  "$cargo_bin" "$@" >/dev/null 2>&1 &
  cargo_pid=$!
  wait "$cargo_pid"
  local rc=$?
  cargo_pid=""
  return $rc
}

rollback() { # reason
  trap '' INT TERM HUP
  local n=${#renamed[@]}
  if undo; then
    refuse "$1; restored $n file(s), nothing left changed: ${renamed[*]:-none}"
  fi
  refuse "$1; could not restore every file, the originals are in $journal; run $SELF --recover"
}
renamed=()

# Phase 1: stage. A failure here has changed nothing yet.
for f in "${files[@]}"; do
  t=$(mktemp "$(dirname -- "$f")/$(basename -- "$f").bump.XXXXXX") ||
    { discard_staged; refuse "cannot create a temporary file beside $f"; }
  staged[$f]=$t
  if [ "$f" = package-lock.json ]; then
    awk -v old="$first" -v new="$target" '
      function swap() { sub("\"version\": \"" old "\"", "\"version\": \"" new "\"") }
      /^  "version": "/ && !o { swap(); o = 1 }
      /^    "": \{/ { r = 1 }
      r && /^      "version": "/ { swap(); r = 0 }
      { print }' "$f" >"$t"
  else
    awk -v old="\"$first\"" -v new="\"$target\"" '
      !done && index($0, old) && /"version": "|^version = "/ {
        sub(old, new); done = 1
      }
      { print }' "$f" >"$t"
  fi || { discard_staged; refuse "cannot stage $f"; }
  chmod --reference="$f" "$t" || { discard_staged; refuse "cannot stage $f"; }
  if [ "$f" = package-lock.json ]; then ok=0; lock_agrees "$t" "$target" && ok=1
  else ok=0; [ "$(read_version "$f" "$t")" = "$target" ] && ok=1; fi
  [ "$ok" = 1 ] || { discard_staged; refuse "cannot stage $f with version $target"; }
done

# Phase 2: write the journal whole, then rename each staged file into place.
jt=$journal.tmp
rm -rf -- "$jt"
mkdir -- "$jt" && : >"$jt/manifest" || { discard_staged; refuse "cannot create $jt"; }
for i in "${!files[@]}"; do
  f=${files[$i]}
  cp -p -- "$f" "$jt/$i" && printf '%s\t%s\n' "$i" "$f" >>"$jt/manifest" ||
    { discard_staged; refuse "cannot back up $f"; }
done
if [ "$lock_existed" = yes ]; then
  cp -p -- Cargo.lock "$jt/lock" && printf 'lock\tCargo.lock\n' >>"$jt/manifest" ||
    { discard_staged; refuse "cannot back up Cargo.lock"; }
else
  printf -- '-\tCargo.lock\n' >>"$jt/manifest"
fi
mv -- "$jt" "$journal" || { discard_staged; refuse "cannot create $journal"; }
for f in "${files[@]}"; do
  mv -f -- "${staged[$f]}" "$f" || rollback "cannot rename the new $f into place"
  renamed+=("$f")
  unset "staged[$f]"
done

# Phase 3: refresh Cargo.lock and read every file back.
run_cargo metadata --format-version 1 ||
  rollback "cargo metadata failed; Cargo.lock is not refreshed"
for f in "${files[@]}"; do
  if [ "$f" = package-lock.json ]; then
    lock_agrees "$f" "$target" || rollback "$f does not carry $target in both root entries after the rewrite"
  else
    got=$(read_version "$f" "$f")
    [ "$got" = "$target" ] || rollback "$f reads $got after the rewrite, expected $target"
  fi
done
run_cargo metadata --locked --format-version 1 ||
  rollback "Cargo.lock does not match the manifests after the bump"

trap '' INT TERM HUP
commit_journal
echo "$SELF: all ${#files[@]} files and Cargo.lock now declare $target; nothing committed or tagged"
