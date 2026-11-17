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
#       .release-bump-journal/ in the repository root: a copy of every original
#       file and a manifest with the SHA-256 of each original and of the content
#       about to replace it. A run killed without a chance to clean up (SIGKILL,
#       power loss) leaves it. While it exists every mode refuses to run. --recover
#       first checks every file: one still holding its original is left alone, one
#       holding exactly what the run wrote is restored, and anything else (an edit
#       made after the crash, a deleted file) makes it refuse, change nothing, and
#       print how to finish by hand. Cargo.lock counts as written by the run only
#       once its hash was recorded after cargo returned; before that a changed
#       Cargo.lock is refused too. A missing, truncated or malformed journal is
#       refused, never guessed at. Each restore is a rename, so an interrupted
#       --recover can be run again. A successful or rolled-back run removes the
#       journal itself.
#       Before it looks at any file, --recover takes the journal's writer lock
#       (below) without waiting, and refuses, changing nothing, while any process
#       of the interrupted run still holds it.
#
# Writer lock: --apply holds an exclusive flock(1) on .release-bump-journal/
# writer.lock from before the journal exists until it exits, on descriptor 9,
# which it leaves open across exec. Every process it starts (cargo, and cargo's
# own children such as rustc) inherits that descriptor, so the kernel keeps the
# lock until the last of them exits, even when --apply itself was SIGKILLed. A
# lock file nobody holds, left by a crash, does not block anything. The lock
# names no process, so a reused PID cannot fool it. Limits: a descendant that
# closes descriptor 9 and keeps writing is not covered (cargo and rustc keep it);
# flock(1) must be installed (util-linux on Linux; macOS does not ship it), and
# the lock is advisory and may not work on a network filesystem.
#
# Durability: journal entries, staged files, restored files and the directories
# holding them are fsynced (coreutils sync) before the step that relies on them.
# This orders the writes for a power cut; it has not been tested against one.
#
# Signals: --apply traps SIGINT, SIGTERM and SIGHUP and rolls back (with the same
# checks as --recover). --recover does not trap them; killing it at any point
# leaves a state that running it again resolves.
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
journal_format='release-bump-journal 2'

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
  mv -- "$journal" "$journal.done" && sync -- . ||
    { echo "$SELF: warning: could not retire $journal; remove it by hand" >&2; exit 1; }
  rm -rf -- "$journal.done"
}

# The SHA-256 of a file, "-" when it does not exist, "unreadable" when it cannot
# be read (a directory, no permission): never equal to a recorded hash.
hash_of() { # path
  [ -e "$1" ] || [ -L "$1" ] || { echo -; return; }
  local h
  h=$(sha256sum <"$1" 2>/dev/null) || { echo unreadable; return; }
  echo "${h%% *}"
}

bad_journal() { # reason
  echo "$SELF: refusing: $journal is incomplete or corrupt ($1); nothing was changed." >&2
  echo "  Recover by hand: compare each managed file with the originals kept in $journal, put back what you need, then remove $journal." >&2
  return 1
}

# Reads the journal and decides, without changing anything, what recovery does with
# each file: still the original (leave it), exactly what the run wrote (restore it),
# or anything else (foreign: refuse). Cargo.lock is "what the run wrote" only if
# its hash was recorded after cargo returned, or with $1 = trust-lock (an in-process
# rollback whose own cargo child has exited). Fills todo (backup TAB path),
# leftovers (staged files) and foreign (one line per file); fails, after saying
# why, on a journal that is missing, truncated or malformed.
classify() { # trust-lock|no
  local b p o q s x line cur post lockpost="" n=0 count=""
  todo=(); leftovers=(); foreign=()
  [ -f "$journal/manifest" ] || { bad_journal "no manifest"; return 1; }
  if [ -e "$journal/lock.post" ]; then
    lockpost=$(cat -- "$journal/lock.post") && [[ $lockpost =~ ^[0-9a-f]{64}$ ]] ||
      { bad_journal "lock.post is not a SHA-256"; return 1; }
  fi
  {
    IFS= read -r line && [ "$line" = "$journal_format" ] || { bad_journal "unknown format '$line'"; return 1; }
    while IFS=$'\t' read -r b p o q s x; do
      if [ "$b" = end ] && [ -z "$o" ]; then count=$p; break; fi
      n=$((n + 1))
      [[ -z $x && -n $p && $p != /* && $p != -* && $p != *..* ]] &&
        [[ $o =~ ^[0-9a-f]{64}$|^-$ && $q =~ ^[0-9a-f]{64}$|^-$ ]] &&
        { [ "$q" != - ] || [ "$p" = Cargo.lock ]; } &&
        { [ "$s" = - ] || [[ $s == "$p".bump.?????? ]]; } &&
        if [ "$b" = - ]; then [ "$o" = - ] && [ "$p" = Cargo.lock ]; else [[ $b =~ ^[0-9]+$|^lock$ ]] && [ "$(hash_of "$journal/$b")" = "$o" ]; fi ||
        { bad_journal "entry $n is malformed or its original copy does not match"; return 1; }
      [ "$s" = - ] || leftovers+=("$s")
      post=$q
      [ "$p" = Cargo.lock ] && post=${lockpost:--}
      cur=$(hash_of "$p")
      if [ "$cur" = "$o" ]; then
        continue
      elif { [ "$post" != - ] && [ "$cur" = "$post" ]; } || { [ "$p" = Cargo.lock ] && [ "$1" = trust-lock ]; }; then
        todo+=("$b"$'\t'"$p")
      elif [ "$cur" = - ]; then
        foreign+=("$p"$'\t'"$b"$'\t'"was deleted after the run stopped")
      elif [ "$post" = - ]; then
        foreign+=("$p"$'\t'"$b"$'\t'"differs from its original, and the run stopped before recording what cargo wrote, so a cargo change cannot be told from an edit")
      else
        foreign+=("$p"$'\t'"$b"$'\t'"differs from both its original and what the run wrote")
      fi
    done
  } <"$journal/manifest"
  [ -n "$count" ] && [ "$count" = "$n" ] || { bad_journal "manifest ends early"; return 1; }
}

# Explains a refusal caused by foreign files, with the commands to finish by hand.
report_foreign() {
  local e p b
  echo "$SELF: refusing: automatic recovery would overwrite changes it did not make; nothing was changed." >&2
  for e in "${foreign[@]}"; do echo "  ${e%%$'\t'*} ${e##*$'\t'}" >&2; done
  echo "To finish by hand, either put back the original of each file above and run $SELF --recover again:" >&2
  for e in "${foreign[@]}"; do
    p=${e%%$'\t'*}; e=${e#*$'\t'}; b=${e%%$'\t'*}
    if [ "$b" = - ]; then echo "    rm -- '$p'    # it did not exist before the run" >&2
    else echo "    cp -p -- '$journal/$b' '$p'" >&2; fi
  done
  echo "or keep the tree as it is: remove $journal yourself and reconcile the versions by hand." >&2
}

# Restores every file in todo through a fsynced temporary and a rename, so a kill
# leaves each file either as the run wrote it or as the original, then removes the
# staged files. Succeeds only if every file was restored.
restore_files() {
  local e b p s
  for e in "${todo[@]}"; do
    b=${e%%$'\t'*}; p=${e#*$'\t'}
    if [ "$b" = - ]; then
      rm -f -- "$p" || return 1
    else
      cp -p -- "$journal/$b" "$p.release-bump-restore" && sync -- "$p.release-bump-restore" &&
        mv -f -- "$p.release-bump-restore" "$p" || return 1
    fi
    sync -- "$(dirname -- "$p")" || return 1
  done
  for s in "${leftovers[@]}"; do rm -f -- "$s"; done
}

# Restores from the journal and removes it. Keeps it, after saying why, when the
# journal is unreadable or a file changed outside the run.
undo() { # trust-lock|no
  classify "$1" || return 1
  [ "${#foreign[@]}" -eq 0 ] || { report_foreign; return 1; }
  restore_files && commit_journal
}

writer_lock=$journal/writer.lock
have_flock() {
  command -v flock >/dev/null 2>&1 ||
    refuse "flock (util-linux) is required to tell whether a process of an interrupted run is still running"
}

if [ "$mode" = --recover ]; then
  [ -e "$journal" ] || refuse "no $journal here; nothing to recover"
  have_flock
  { [ -f "$writer_lock" ] && exec 9<>"$writer_lock"; } 2>/dev/null || { bad_journal "no writer lock"; exit 1; }
  flock -n 9 ||
    refuse "a process of the interrupted run (cargo or a child of it) still holds $writer_lock and could still change Cargo.lock; nothing was changed. Wait for it to exit (fuser -v $writer_lock names it), then run $SELF --recover again"
  classify no || exit 1
  [ "${#foreign[@]}" -eq 0 ] || { report_foreign; exit 1; }
  restore_files || refuse "could not restore every file; the originals remain in $journal; run $SELF --recover again"
  commit_journal
  echo "$SELF: restored ${#todo[@]} file(s) from $journal and removed it"
  exit 0
fi
[ ! -e "$journal" ] ||
  refuse "$journal exists: an earlier run did not finish. Run $SELF --recover to restore the original files"
have_flock

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
# A journal a successful run committed but did not finish deleting; only a mode
# that changes files removes it.
[ ! -e "$journal.done" ] || rm -rf -- "$journal.done"

# --apply ---------------------------------------------------------------------
# Three phases. 1: every rewritten file is written to a sibling temporary file and
# checked; nothing in the tree changes. 2: the journal is written (a copy of every
# original, then renamed into place whole), and each staged file is renamed over
# its original. 3: cargo metadata refreshes Cargo.lock and every file is read back.
# A failure or signal in phase 2 or 3 restores everything from the journal.
# Every rename is preceded by a fsynced record of both the original and the new
# content's hash, so wherever a crash falls each file is one of the two; no
# journal update is needed between renames.
declare -A staged=()
cargo_pid=""
lock_existed=no
[ -e Cargo.lock ] && lock_existed=yes

discard_staged() {
  local f
  for f in "${!staged[@]}"; do rm -f -- "${staged[$f]}"; done
  rm -rf -- "$journal.tmp"
}

# A signal runs the same recovery as a failure, then exits 128+N.
on_signal() { # status name
  trap '' INT TERM HUP
  if [ -n "$cargo_pid" ]; then kill "$cargo_pid" 2>/dev/null; wait "$cargo_pid" 2>/dev/null; fi
  if [ ! -d "$journal" ]; then
    discard_staged
    echo "$SELF: stopped by $2 before any file was changed" >&2
  elif undo trust-lock; then
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
  if undo trust-lock; then
    refuse "$1; restored $n file(s), nothing left changed: ${renamed[*]:-none}"
  fi
  refuse "$1; could not restore every file, $journal is kept; run $SELF --recover"
}
renamed=()

# Phase 1: stage. A failure here has changed nothing yet.
for f in "${files[@]}"; do
  t=$(mktemp -- "$f.bump.XXXXXX") ||
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
sync -- "${staged[@]}" || { discard_staged; refuse "cannot flush the staged files"; }

# Phase 2: write the journal whole, then rename each staged file into place. A
# manifest line is: backup copy, path, original hash, new hash, staged file ("-"
# for none; Cargo.lock's new hash goes to lock.post once cargo has written it).
jt=$journal.tmp
rm -rf -- "$jt"
mkdir -- "$jt" && echo "$journal_format" >"$jt/manifest" || { discard_staged; refuse "cannot create $jt"; }
# The writer lock (see the header). It is taken before the journal exists and
# before cargo starts; descriptor 9 stays open across exec on purpose.
{ exec 9>"$jt/writer.lock"; } 2>/dev/null && flock -n 9 || { discard_staged; refuse "cannot lock $jt/writer.lock"; }
for i in "${!files[@]}"; do
  f=${files[$i]}
  cp -p -- "$f" "$jt/$i" &&
    printf '%s\t%s\t%s\t%s\t%s\n' "$i" "$f" "$(hash_of "$jt/$i")" "$(hash_of "${staged[$f]}")" "${staged[$f]}" >>"$jt/manifest" ||
    { discard_staged; refuse "cannot back up $f"; }
done
if [ "$lock_existed" = yes ]; then
  cp -p -- Cargo.lock "$jt/lock" && printf 'lock\tCargo.lock\t%s\t-\t-\n' "$(hash_of "$jt/lock")" >>"$jt/manifest" ||
    { discard_staged; refuse "cannot back up Cargo.lock"; }
else
  printf -- '-\tCargo.lock\t-\t-\t-\n' >>"$jt/manifest"
fi
printf 'end\t%s\n' $((${#files[@]} + 1)) >>"$jt/manifest" && sync -- "$jt"/* "$jt" &&
  mv -- "$jt" "$journal" && sync -- . || { discard_staged; refuse "cannot create $journal"; }
for f in "${files[@]}"; do
  mv -f -- "${staged[$f]}" "$f" || rollback "cannot rename the new $f into place"
  renamed+=("$f")
  unset "staged[$f]"
done

# Phase 3: refresh Cargo.lock and read every file back.
run_cargo metadata --format-version 1 ||
  rollback "cargo metadata failed; Cargo.lock is not refreshed"
# Records what cargo wrote, so --recover can tell it from a later edit.
if [ -e Cargo.lock ]; then
  sync -- Cargo.lock . && hash_of Cargo.lock >"$journal/lock.post.tmp" && sync -- "$journal/lock.post.tmp" &&
    mv -- "$journal/lock.post.tmp" "$journal/lock.post" && sync -- "$journal" ||
    rollback "cannot record the refreshed Cargo.lock in $journal"
fi
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
declare -A dirs=()
for f in "${files[@]}"; do dirs[$(dirname -- "$f")]=1; done
sync -- "${!dirs[@]}" || rollback "cannot flush the renamed files"
commit_journal
echo "$SELF: all ${#files[@]} files and Cargo.lock now declare $target; nothing committed or tagged"
