#!/usr/bin/env bash
# SPDX-License-Identifier: MPL-2.0
# Hermetic test of scripts/release-bump.sh in disposable fixture repositories.
# Cargo is a stub (SLASHIT_RELEASE_CARGO, kept so each run can pick its own stub),
# so nothing is compiled and no lockfile is rewritten by a real resolver. A rename
# failure, or a SIGKILL just before or after one rename, is injected with a
# test-only mv shim first on PATH.
#
# usage: scripts/test-release-bump.sh
set -uo pipefail
export LC_ALL=C

src=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/release-bump.sh
work=$(mktemp -d "${TMPDIR:-/tmp}/release-bump-test.XXXXXX") || { echo "cannot create a temporary directory" >&2; exit 1; }
# Every rm below runs on a path under $work, so $work must be a real directory.
case $work in /?*/?*) [ -d "$work" ] || { echo "temporary directory $work is missing" >&2; exit 1; } ;;
  *) echo "refusing to use '$work' as the temporary directory" >&2; exit 1 ;; esac
# Every process a test starts names $work on its command line (stubs, background
# runs, lock holders), so it can be found, killed and checked for at exit.
cleanup() {
  pkill -KILL -f -- "$work/" 2>/dev/null
  for _ in $(seq 50); do pgrep -f -- "$work/" >/dev/null || break; sleep 0.1; done
  pgrep -f -- "$work/" >/dev/null && echo "warning: processes from $work survived SIGKILL" >&2
  rm -rf -- "$work"
}
trap cleanup EXIT
real_mv=$(command -v mv)
real_cp=$(command -v cp)
pass=0
fails=0
ok() { pass=$((pass + 1)); echo "ok   $1"; }
bad() { fails=$((fails + 1)); echo "FAIL $1"; }

cat >"$work/cargo-ok" <<'STUB'
#!/usr/bin/env bash
exit 0
STUB
cat >"$work/cargo-fail" <<'STUB'
#!/usr/bin/env bash
exit 1
STUB
# Writes Cargo.lock, then fails: the script must undo what cargo did.
cat >"$work/cargo-lock-fail" <<'STUB'
#!/usr/bin/env bash
echo "written by cargo" >Cargo.lock
exit 1
STUB
# Announces itself, then sleeps until killed (at most 30 s).
cat >"$work/cargo-sleep" <<STUB
#!/usr/bin/env bash
echo "written by cargo" >Cargo.lock
: >"$work/started"
for _ in \$(seq 300); do sleep 0.1; done
STUB
# Announces itself, waits (at most 10 s) for $work/go, then writes Cargo.lock: a
# cargo that outlives the SIGKILLed bump and changes the lockfile afterwards.
cat >"$work/cargo-orphan" <<STUB
#!/usr/bin/env bash
: >"$work/started"
for _ in \$(seq 100); do [ -e "$work/go" ] && break; sleep 0.1; done
[ -e "$work/go" ] && echo "written by the orphaned cargo" >Cargo.lock
STUB
# Kills its parent (the bump script) outright, so no trap runs.
cat >"$work/cargo-crash" <<'STUB'
#!/usr/bin/env bash
echo "written by cargo" >Cargo.lock
kill -9 "$PPID"
STUB
# Writes Cargo.lock and succeeds.
cat >"$work/cargo-write" <<'STUB'
#!/usr/bin/env bash
echo "written by cargo" >Cargo.lock
STUB
# Writes Cargo.lock; the --locked check that follows kills the bump script.
cat >"$work/cargo-crash-locked" <<'STUB'
#!/usr/bin/env bash
case " $* " in *" --locked "*) kill -9 "$PPID"; exit 1 ;; esac
echo "written by cargo" >Cargo.lock
STUB
chmod +x "$work"/cargo-*
# A mv that fails when the destination is $MV_FAIL_TARGET, and SIGKILLs its parent
# (the script) $MV_KILL (before|after) renaming onto $MV_KILL_TARGET.
mkdir "$work/shim"
cat >"$work/shim/mv" <<'STUB'
#!/usr/bin/env bash
for last; do :; done
[ "$last" = "${MV_FAIL_TARGET-}" ] && exit 1
if [ "$last" = "${MV_KILL_TARGET-}" ]; then
  [ "$MV_KILL" = before ] && { kill -9 "$PPID"; exit 1; }
  "$REAL_MV" "$@"; st=$?; kill -9 "$PPID"; exit "$st"
fi
exec "$REAL_MV" "$@"
STUB
chmod +x "$work/shim/mv"
# A cp that, only in the run whose environment names $CP_GATE, pauses its first
# copy into the journal's staging directory until $CP_GATE.go exists (at most
# 20 s): that run holds its staging lock while a second run starts.
cat >"$work/shim/cp" <<STUB
#!/usr/bin/env bash
case " \$* " in
  *" .release-bump-journal.tmp/"*)
    if [ -n "\${CP_GATE:-}" ]; then
      : >"\$CP_GATE.paused"
      for _ in \$(seq 200); do [ -e "\$CP_GATE.go" ] && break; sleep 0.1; done
    fi ;;
esac
exec "$real_cp" "\$@"
STUB
chmod +x "$work/shim/cp"

# A fixture repository at version $1, with a curated section for 0.0.4 and an
# empty one for 0.0.5.
make_repo() { # dir version
  local d=$1 v=$2
  case $d in "$work"/?*) ;; *) echo "make_repo: $d is outside $work" >&2; exit 1 ;; esac
  rm -rf -- "$d"
  mkdir -p "$d/src-tauri" "$d/crates/a" "$d/crates/b"
  printf '{\n  "productName": "x",\n  "version": "%s",\n  "identifier": "y"\n}\n' "$v" >"$d/src-tauri/tauri.conf.json"
  printf '{\n  "name": "e2e",\n  "version": "%s"\n}\n' "$v" >"$d/package.json"
  cat >"$d/package-lock.json" <<LOCK
{
  "name": "e2e",
  "version": "$v",
  "lockfileVersion": 3,
  "packages": {
    "": {
      "name": "e2e",
      "version": "$v"
    },
    "node_modules/dep": {
      "version": "1.0.0"
    }
  }
}
LOCK
  printf '[package]\nname = "root"\nversion = "%s"\n' "$v" >"$d/Cargo.toml"
  printf '[package]\nname = "ui"\nversion = "%s"\n' "$v" >"$d/src-tauri/Cargo.toml"
  printf '[package]\nname = "a"\nversion = "%s"\n' "$v" >"$d/crates/a/Cargo.toml"
  printf '[package]\nname = "b"\nversion = "%s"\n' "$v" >"$d/crates/b/Cargo.toml"
  cat >"$d/CHANGELOG.md" <<'CL'
# Changelog

## [Unreleased]

## [0.0.5]

## [0.0.4]

### Added

- A curated entry.

## [0.0.3]

- Earlier.
CL
}

digest() { # dir: every file's content, in order
  (cd "$1" && find . -type f ! -name .release-bump.lock -print0 | sort -z | xargs -0 sha256sum)
}

repo=$work/repo
make_repo "$repo" 0.0.3
run() { # args...: run the script in the fixture with the stub cargo
  SLASHIT_RELEASE_CARGO=${CARGO_STUB:-$work/cargo-ok} bash "$src" "$@" "$repo" 2>&1
}
# Runs the script with the mv shim SIGKILLing it $1 (before|after) the rename onto
# $2; the subshell keeps the shell's "Killed" notice out of the output.
killed_at() { # before|after destination args...
  local when=$1 dest=$2; shift 2
  (PATH="$work/shim:$PATH" REAL_MV="$real_mv" MV_KILL="$when" MV_KILL_TARGET="$dest" run "$@") >/dev/null 2>&1
  settle
}
# A stub that SIGKILLed the script inherited its writer lock and may still be
# exiting; waits, at most 20 s, until nothing holds the lock.
settle() {
  local l=$repo/.release-bump-journal/writer.lock
  [ -e "$l" ] || return 0
  flock -w 20 "$l" true || bad "a process of the killed run still holds $l after 20 s"
}
journal_left() { [ -d "$repo/.release-bump-journal" ]; }
# Runs the hand-recovery command that --recover printed (in $out) for file $1.
follow_instruction() { # path
  local cmd
  cmd=$(grep -E "^    (cp -p -- '[^']*' |rm -- )'$1'" <<<"$out")
  [ -n "$cmd" ] && (cd "$repo" && eval "$cmd")
}

echo "== dry run is read-only and reports the bump"
before=$(digest "$repo")
out=$(run --dry-run 0.0.4); st=$?
if [ "$st" -eq 0 ] && [[ "$out" == *"0.0.3 -> 0.0.4"* ]] && [ "$before" = "$(digest "$repo")" ] && [ ! -e "$repo/.release-bump.lock" ]; then
  ok "--dry-run reports the bump, changes nothing and creates no lock file"
else bad "--dry-run did not behave read-only (exit $st)"; printf '%s\n' "$out" >&2; fi

echo "== apply rewrites every version and nothing else"
out=$(run --apply 0.0.4); st=$?
left=$(grep -rl '0\.0\.3' "$repo" --exclude-dir=.git | grep -v CHANGELOG || true)
dep=$(grep -c '"node_modules/dep"' "$repo/package-lock.json")
depv=$(grep -c "\"version\": \"0.0.4\"" "$repo/package-lock.json")
depd=$(grep -c "\"version\": \"1.0.0\"" "$repo/package-lock.json")
if [ "$st" -eq 0 ] && [ -z "$left" ] && [ "$dep" -eq 1 ] && [ "$depv" -eq 2 ] && [ "$depd" -eq 1 ] &&
  grep -q '"version": "0.0.4"' "$repo/src-tauri/tauri.conf.json" && grep -q '^version = "0.0.4"' "$repo/Cargo.toml" &&
  grep -q '^version = "0.0.4"' "$repo/crates/b/Cargo.toml" && grep -q '"version": "0.0.4"' "$repo/package.json"; then
  ok "every manifest and both root lock entries move to 0.0.4; the dependency entry and the CHANGELOG are not touched"
else bad "apply produced the wrong result (exit $st, left: ${left:-none}, lock versions $depv)"; printf '%s\n' "$out" >&2; fi
grep -q '"version": "0.0.4"' "$repo/CHANGELOG.md" 2>/dev/null && bad "CHANGELOG was rewritten"

echo "== refusals"
make_repo "$repo" 0.0.3
sed -i 's/"version": "0.0.3"/"version": "0.0.2"/' "$repo/package.json"
before=$(digest "$repo")
out=$(run --apply 0.0.4); st=$?
if [ "$st" -eq 1 ] && [[ "$out" == *"reconcile them first"* ]] && [ "$before" = "$(digest "$repo")" ]; then
  ok "disagreeing files are refused before any file is changed"
else bad "disagreeing versions were not refused cleanly (exit $st)"; printf '%s\n' "$out" >&2; fi

make_repo "$repo" 0.0.3
out=$(run --apply 0.0.5); st=$?
if [ "$st" -eq 1 ] && [[ "$out" == *"no curated section"* ]]; then ok "an empty CHANGELOG section for the target is refused"
else bad "empty changelog section was not refused (exit $st)"; printf '%s\n' "$out" >&2; fi

out=$(run --apply 0.0.9-rc1); st=$?
if [ "$st" -eq 2 ]; then ok "a prerelease or non-plain version is a usage error"
else bad "invalid version not rejected (exit $st)"; fi

out=$(run --apply 0.0.3); st=$?
if [ "$st" -eq 1 ] && [[ "$out" == *"already declare"* ]]; then ok "a bump to the current version is refused"
else bad "same-version bump not refused (exit $st)"; fi

echo "== a failing cargo is refused and every file is restored"
make_repo "$repo" 0.0.3
before=$(digest "$repo")
out=$(CARGO_STUB="$work/cargo-fail" run --apply 0.0.4); st=$?
if [ "$st" -eq 1 ] && [[ "$out" == *"Cargo.lock is not refreshed"* ]] && [ "$before" = "$(digest "$repo")" ]; then
  ok "a cargo failure is reported, the command does not claim success, and no file is left changed"
else bad "cargo failure was not rolled back (exit $st)"; printf '%s\n' "$out" >&2; fi

echo "== a rename failing partway through restores the files already renamed"
make_repo "$repo" 0.0.3
before=$(digest "$repo")
out=$(PATH="$work/shim:$PATH" REAL_MV="$real_mv" MV_FAIL_TARGET=package-lock.json run --apply 0.0.4); st=$?
if [ "$st" -eq 1 ] && [[ "$out" == *"restored 2 file(s)"* ]] && [ "$before" = "$(digest "$repo")" ]; then
  ok "a failed rename restores the two files renamed before it, and no staged file is left behind"
else bad "rename failure was not rolled back (exit $st)"; printf '%s\n' "$out" >&2; fi

echo "== package-lock.json: both root entries must agree"
make_repo "$repo" 0.0.3
sed -i '/^      "version": "0.0.3"/s/0\.0\.3/0.0.2/' "$repo/package-lock.json"
before=$(digest "$repo")
out=$(run --apply 0.0.4); st=$?
if [ "$st" -eq 1 ] && [[ "$out" == *'packages[""]'* ]] && grep -q '"version": "0.0.2"' "$repo/package-lock.json" && [ "$before" = "$(digest "$repo")" ]; then
  ok "outer 0.0.3 with packages[\"\"] 0.0.2 is refused and no file changes"
else bad "lock entries disagreeing were not refused cleanly (exit $st)"; printf '%s\n' "$out" >&2; fi
out=$(run --dry-run 0.0.4); st=$?
[ "$st" -eq 1 ] && ok "--dry-run refuses it too" || bad "--dry-run accepted disagreeing lock entries (exit $st)"

echo "== cargo that writes Cargo.lock and then fails"
for have in with without; do
  make_repo "$repo" 0.0.3
  [ "$have" = with ] && printf 'original lock\n' >"$repo/Cargo.lock"
  before=$(digest "$repo")
  out=$(CARGO_STUB="$work/cargo-lock-fail" run --apply 0.0.4); st=$?
  if [ "$st" -eq 1 ] && [ "$before" = "$(digest "$repo")" ]; then
    ok "$have an existing Cargo.lock: the whole fixture digest is unchanged after the failure"
  else bad "$have Cargo.lock: not restored (exit $st)"; printf '%s\n' "$out" >&2; fi
done
[ ! -e "$repo/Cargo.lock" ] && ok "a Cargo.lock that did not exist is not left behind" || bad "Cargo.lock was left behind"

echo "== a signal during cargo restores everything and exits non-zero"
# Job control, so the background run does not start with SIGINT ignored.
set -m
for sig in INT TERM HUP; do
  make_repo "$repo" 0.0.3
  printf 'original lock\n' >"$repo/Cargo.lock"
  before=$(digest "$repo")
  rm -f "$work/started"
  SLASHIT_RELEASE_CARGO="$work/cargo-sleep" bash "$src" --apply 0.0.4 "$repo" >"$work/sig.out" 2>&1 &
  pid=$!
  for _ in $(seq 100); do [ -e "$work/started" ] && break; sleep 0.1; done
  [ -e "$work/started" ] || bad "SIG$sig: cargo stub never started"
  [ "$before" != "$(digest "$repo")" ] || bad "SIG$sig: the bump had not started changing files"
  kill -"$sig" "$pid"; wait "$pid"; st=$?
  if [ "$st" -ne 0 ] && [ "$before" = "$(digest "$repo")" ] && grep -q "stopped by" "$work/sig.out"; then
    ok "SIG$sig mid-cargo: exit $st, fixture digest unchanged, no journal left"
  else bad "SIG$sig did not restore (exit $st)"; cat "$work/sig.out" >&2; fi
done
set +m

echo "== a run killed inside cargo leaves a journal; --recover will not guess who changed Cargo.lock"
for have in with without; do
  make_repo "$repo" 0.0.3
  [ "$have" = with ] && printf 'original lock\n' >"$repo/Cargo.lock"
  before=$(digest "$repo")
  (CARGO_STUB="$work/cargo-crash" run --apply 0.0.4) >/dev/null 2>&1; settle
  journal_left && [ "$before" != "$(digest "$repo")" ] &&
    ok "$have Cargo.lock: a SIGKILLed run leaves the journal and changed files" || bad "$have Cargo.lock: the crash did not leave a journal"
  crashed=$(digest "$repo")
  if [ "$have" = with ]; then
    out=$(run --apply 0.0.5); st=$?
    if [ "$st" -eq 1 ] && [[ "$out" == *"--recover"* ]] && [ "$crashed" = "$(digest "$repo")" ]; then
      ok "a later apply refuses and points to --recover without touching anything"
    else bad "apply was not blocked by the journal (exit $st)"; printf '%s\n' "$out" >&2; fi
    out=$(run --dry-run 0.0.4); st=$?
    [ "$st" -eq 1 ] && ok "so does --dry-run" || bad "--dry-run ignored the journal (exit $st)"
  fi
  for attempt in first repeated; do
    out=$(run --recover); st=$?
    if [ "$st" -eq 1 ] && [[ "$out" == *"Cargo.lock differs from its original, and the run stopped before recording"* ]] &&
      [ "$crashed" = "$(digest "$repo")" ]; then
      ok "$have Cargo.lock, $attempt --recover: refuses the unrecorded Cargo.lock and changes nothing"
    else bad "$have Cargo.lock, $attempt --recover did not refuse cleanly (exit $st)"; printf '%s\n' "$out" >&2; fi
  done
  follow_instruction Cargo.lock && out=$(run --recover); st=$?
  if [ "$st" -eq 0 ] && [ "$before" = "$(digest "$repo")" ]; then
    ok "$have Cargo.lock: after the printed command for Cargo.lock, --recover restores the original fixture digest"
  else bad "$have Cargo.lock: the printed instruction did not lead to recovery (exit $st)"; printf '%s\n' "$out" >&2; fi
done
out=$(run --recover); st=$?
[ "$st" -eq 1 ] && ok "--recover with no journal refuses" || bad "--recover without a journal returned $st"

echo "== an edit made after a SIGKILL is never overwritten by --recover"
# Killed just after Cargo.toml is renamed: tauri.conf.json, package.json,
# package-lock.json and Cargo.toml hold the new content, the crates the original.
for change in edit-written delete-written edit-unreached; do
  make_repo "$repo" 0.0.3
  printf 'original lock\n' >"$repo/Cargo.lock"
  before=$(digest "$repo")
  killed_at after Cargo.toml --apply 0.0.4
  case $change in
    edit-written) f=package.json; sed -i 's/"name": "e2e"/"name": "operator"/' "$repo/$f"; want="$f differs from both" ;;
    delete-written) f=package.json; rm -- "${repo:?}/package.json"; want="$f was deleted" ;;
    edit-unreached) f=crates/b/Cargo.toml; printf '# operator\n' >>"$repo/$f"; want="$f differs from both" ;;
  esac
  edited=$(digest "$repo")
  for attempt in first repeated; do
    out=$(run --recover); st=$?
    if [ "$st" -eq 1 ] && [[ "$out" == *"$want"* ]] && [[ "$out" == *"nothing was changed"* ]] && journal_left &&
      [ "$edited" = "$(digest "$repo")" ]; then
      ok "$change ($f), $attempt --recover: refuses, names the file, and the whole fixture digest is unchanged"
    else bad "$change ($f), $attempt --recover overwrote or did not refuse (exit $st)"; printf '%s\n' "$out" >&2; fi
  done
  follow_instruction "$f" && out=$(run --recover); st=$?
  if [ "$st" -eq 0 ] && [ "$before" = "$(digest "$repo")" ]; then
    ok "$change: after the printed command for $f, --recover restores the rest"
  else bad "$change: the printed instruction did not lead to recovery (exit $st)"; printf '%s\n' "$out" >&2; fi
done

echo "== SIGKILL just before or just after each rename, then --recover"
for f in src-tauri/tauri.conf.json package.json package-lock.json Cargo.toml src-tauri/Cargo.toml crates/a/Cargo.toml crates/b/Cargo.toml; do
  for when in before after; do
    make_repo "$repo" 0.0.3
    printf 'original lock\n' >"$repo/Cargo.lock"
    before=$(digest "$repo")
    killed_at "$when" "$f" --apply 0.0.4
    journal_left || bad "$when $f: no journal left"
    out=$(run --recover); st=$?
    if [ "$st" -eq 0 ] && [ "$before" = "$(digest "$repo")" ]; then
      ok "killed $when renaming $f: --recover restores the original fixture digest (no staged file left)"
    else bad "killed $when renaming $f: not recovered (exit $st)"; printf '%s\n' "$out" >&2; fi
  done
done

echo "== SIGKILL around the journal's own rename"
make_repo "$repo" 0.0.3
killed_at before .release-bump-journal --apply 0.0.4
out=$(run --recover); st=$?
v=$(grep -rl '0\.0\.4' "$repo" --include='*.json' --include='*.toml' | grep -v '\.bump\.' || true)
if [ "$st" -eq 1 ] && [[ "$out" == *"nothing to recover"* ]] && [ -z "$v" ] && run --apply 0.0.4 >/dev/null; then
  ok "killed before the journal exists: no managed file changed, and a new --apply succeeds"
else bad "killed before the journal: unexpected state (exit $st, changed: ${v:-none})"; printf '%s\n' "$out" >&2; fi
make_repo "$repo" 0.0.3
printf 'original lock\n' >"$repo/Cargo.lock"
before=$(digest "$repo")
killed_at after .release-bump-journal --apply 0.0.4
out=$(run --recover); st=$?
if [ "$st" -eq 0 ] && [ "$before" = "$(digest "$repo")" ]; then ok "killed right after the journal is committed: --recover restores the original digest"
else bad "killed after the journal commit: not recovered (exit $st)"; printf '%s\n' "$out" >&2; fi

echo "== Cargo.lock written by cargo: recorded, restored, and still protected"
for when in before after; do
  make_repo "$repo" 0.0.3
  printf 'original lock\n' >"$repo/Cargo.lock"
  before=$(digest "$repo")
  CARGO_STUB="$work/cargo-write" killed_at "$when" .release-bump-journal/lock.post --apply 0.0.4
  out=$(run --recover); st=$?
  if [ "$when" = before ] && [ "$st" -eq 1 ] && [[ "$out" == *"Cargo.lock differs from its original, and the run stopped before recording"* ]]; then
    ok "killed after cargo wrote Cargo.lock but before its hash was recorded: refused as ambiguous"
  elif [ "$when" = after ] && [ "$st" -eq 0 ] && [ "$before" = "$(digest "$repo")" ]; then
    ok "killed right after its hash was recorded: --recover restores the original Cargo.lock"
  else bad "killed $when recording Cargo.lock: wrong outcome (exit $st)"; printf '%s\n' "$out" >&2; fi
done
make_repo "$repo" 0.0.3
before=$(digest "$repo")
(CARGO_STUB="$work/cargo-crash-locked" run --apply 0.0.4) >/dev/null 2>&1; settle
out=$(run --recover); st=$?
if [ "$st" -eq 0 ] && [ "$before" = "$(digest "$repo")" ] && [ ! -e "$repo/Cargo.lock" ]; then
  ok "a Cargo.lock that cargo created, recorded before the crash, is removed by --recover"
else bad "a recorded new Cargo.lock was not removed (exit $st)"; printf '%s\n' "$out" >&2; fi
make_repo "$repo" 0.0.3
printf 'original lock\n' >"$repo/Cargo.lock"
(CARGO_STUB="$work/cargo-crash-locked" run --apply 0.0.4) >/dev/null 2>&1; settle
printf 'operator lock\n' >"$repo/Cargo.lock"
edited=$(digest "$repo")
out=$(run --recover); st=$?
if [ "$st" -eq 1 ] && [[ "$out" == *"Cargo.lock differs from both"* ]] && [ "$edited" = "$(digest "$repo")" ]; then
  ok "an edit to Cargo.lock after its hash was recorded is refused and kept"
else bad "an edited recorded Cargo.lock was overwritten (exit $st)"; printf '%s\n' "$out" >&2; fi
make_repo "$repo" 0.0.3
out=$(CARGO_STUB="$work/cargo-write" run --apply 0.0.4); st=$?
if [ "$st" -eq 0 ] && grep -q 'written by cargo' "$repo/Cargo.lock" && ! journal_left; then
  ok "a successful apply keeps the Cargo.lock cargo generated"
else bad "generated Cargo.lock was not kept (exit $st)"; printf '%s\n' "$out" >&2; fi

echo "== an incomplete or corrupt journal is refused and nothing changes"
for damage in no-manifest no-end-line truncated bad-copy missing-copy bad-lock-post old-format outside-path; do
  make_repo "$repo" 0.0.3
  printf 'original lock\n' >"$repo/Cargo.lock"
  if [ "$damage" = bad-lock-post ]; then (CARGO_STUB="$work/cargo-crash-locked" run --apply 0.0.4) >/dev/null 2>&1; settle
  else killed_at after Cargo.toml --apply 0.0.4; fi
  j=${repo:?}/.release-bump-journal
  case $damage in
    no-manifest) rm -- "${j:?}/manifest" ;;
    no-end-line) sed -i '$d' "$j/manifest" ;;
    truncated) sed -i '4,$d' "$j/manifest" ;;
    bad-copy) printf 'x\n' >>"$j/1" ;;
    missing-copy) rm -- "${j:?}/2" ;;
    bad-lock-post) printf 'garbage\n' >"$j/lock.post" ;;
    old-format) sed -i '1s/.*/release-bump-journal 1/' "$j/manifest" ;;
    outside-path) sed -i 's#\tpackage\.json\(\t\|$\)#\t../outside\1#' "$j/manifest" ;;
  esac
  damaged=$(digest "$repo")
  out=$(run --recover); st=$?
  if [ "$st" -eq 1 ] && [[ "$out" == *"incomplete or corrupt"* ]] && [ "$damaged" = "$(digest "$repo")" ] && [ ! -e "$work/outside" ]; then
    ok "$damage: --recover refuses and the whole fixture digest is unchanged"
  else bad "$damage: --recover guessed (exit $st)"; printf '%s\n' "$out" >&2; fi
done

echo "== --recover killed partway through can be run again"
for when in before after; do
  make_repo "$repo" 0.0.3
  printf 'original lock\n' >"$repo/Cargo.lock"
  before=$(digest "$repo")
  killed_at after Cargo.toml --apply 0.0.4
  killed_at "$when" package.json --recover
  if ! journal_left; then bad "killed $when restoring package.json: the first --recover was not interrupted"; continue; fi
  out=$(run --recover); st=$?
  if [ "$st" -eq 0 ] && [ "$before" = "$(digest "$repo")" ]; then
    ok "--recover killed $when restoring package.json: a second --recover finishes and the original digest is back"
  else bad "--recover killed $when restoring package.json: second run failed (exit $st)"; printf '%s\n' "$out" >&2; fi
done

echo "== a signal during cargo does not overwrite an edit made meanwhile"
make_repo "$repo" 0.0.3
rm -f "$work/started"
SLASHIT_RELEASE_CARGO="$work/cargo-sleep" bash "$src" --apply 0.0.4 "$repo" >"$work/sig.out" 2>&1 &
pid=$!
for _ in $(seq 100); do [ -e "$work/started" ] && break; sleep 0.1; done
sed -i 's/"name": "e2e"/"name": "operator"/' "$repo/package.json"
kill -TERM "$pid"; wait "$pid"; st=$?
if [ "$st" -ne 0 ] && grep -q '"name": "operator"' "$repo/package.json" && grep -q 'package.json differs from both' "$work/sig.out" && journal_left; then
  ok "SIGTERM rollback refuses to overwrite package.json edited during the run and keeps the journal"
else bad "SIGTERM rollback overwrote an edit (exit $st)"; cat "$work/sig.out" >&2; fi

echo "== a leftover committed journal (.done): --dry-run leaves it, --apply removes it"
make_repo "$repo" 0.0.3
mkdir -p "$repo/.release-bump-journal.done" && echo junk >"$repo/.release-bump-journal.done/manifest"
before=$(digest "$repo")
out=$(run --dry-run 0.0.4); st=$?
if [ "$st" -eq 0 ] && [[ "$out" == *"0.0.3 -> 0.0.4"* ]] && [ "$before" = "$(digest "$repo")" ] &&
  [ "$(cat "$repo/.release-bump-journal.done/manifest")" = junk ]; then
  ok "--dry-run is not blocked by a .done leftover and leaves every file, the leftover included, byte-identical"
else bad "--dry-run changed a file or was blocked by a .done leftover (exit $st)"; printf '%s\n' "$out" >&2; fi
out=$(run --apply 0.0.4); st=$?
if [ "$st" -eq 0 ] && [ ! -e "$repo/.release-bump-journal.done" ] && grep -q '"version": "0.0.4"' "$repo/src-tauri/tauri.conf.json"; then
  ok "--apply removes the .done leftover, does not restore it, and is not blocked by it"
else bad "--apply mishandled the .done leftover (exit $st)"; printf '%s\n' "$out" >&2; fi

echo "== a cargo left running by a SIGKILLed apply blocks --recover until it exits"
make_repo "$repo" 0.0.3
printf 'original lock\n' >"$repo/Cargo.lock"
before=$(digest "$repo")
rm -f "$work/started" "$work/go"
SLASHIT_RELEASE_CARGO="$work/cargo-orphan" bash "$src" --apply 0.0.4 "$repo" >/dev/null 2>&1 &
pid=$!
for _ in $(seq 100); do [ -e "$work/started" ] && break; sleep 0.1; done
[ -e "$work/started" ] || bad "orphan: cargo stub never started"
kill -KILL "$pid"; wait "$pid" 2>/dev/null
pgrep -f -- "$work/cargo-orphan" >/dev/null || bad "orphan: the cargo stub did not outlive the SIGKILLed apply"
held=$(digest "$repo")
out=$(run --recover); st=$?
if [ "$st" -eq 1 ] && [[ "$out" == *"still holds"* ]] && [[ "$out" == *"nothing was changed"* ]] && journal_left &&
  [ "$held" = "$(digest "$repo")" ]; then
  ok "while the orphaned cargo runs, --recover refuses and the whole fixture digest is unchanged"
else bad "--recover acted while the orphaned cargo was running (exit $st)"; printf '%s\n' "$out" >&2; fi
out=$(run --apply 0.0.4); st=$?
if [ "$st" -eq 1 ] && [[ "$out" == *"holds"* ]] && [ "$held" = "$(digest "$repo")" ]; then
  ok "while the orphaned cargo runs, a new --apply refuses and changes nothing"
else bad "--apply ran while the orphaned cargo was running (exit $st)"; printf '%s\n' "$out" >&2; fi
: >"$work/go"
settle
if grep -q 'written by the orphaned cargo' "$repo/Cargo.lock" && ! pgrep -f -- "$work/cargo-orphan" >/dev/null; then
  ok "the orphaned cargo wrote Cargo.lock after the apply died, then exited"
else bad "the orphaned cargo did not write Cargo.lock and exit"; fi
crashed=$(digest "$repo")
out=$(run --recover); st=$?
if [ "$st" -eq 1 ] && [[ "$out" == *"Cargo.lock differs from its original, and the run stopped before recording"* ]] &&
  [ "$crashed" = "$(digest "$repo")" ]; then
  ok "after it exits, --recover refuses its unrecorded Cargo.lock as ambiguous and changes nothing"
else bad "--recover after the orphan exited did not refuse cleanly (exit $st)"; printf '%s\n' "$out" >&2; fi
follow_instruction Cargo.lock && out=$(run --recover); st=$?
if [ "$st" -eq 0 ] && [ "$before" = "$(digest "$repo")" ]; then
  ok "after the printed command for Cargo.lock, --recover restores the original fixture digest"
else bad "the orphan case did not end in the original digest (exit $st)"; printf '%s\n' "$out" >&2; fi

echo "== the writer lock is held by processes, not named by a PID"
make_repo "$repo" 0.0.3
printf 'original lock\n' >"$repo/Cargo.lock"
before=$(digest "$repo")
killed_at after Cargo.toml --apply 0.0.4
l=$repo/.release-bump-journal/writer.lock
[ -f "$l" ] || bad "a killed apply left no writer lock in the journal"
echo "$$" >"$l" # a live PID in a lock file nobody holds must not matter
out=$(run --recover); st=$?
if [ "$st" -eq 0 ] && [ "$before" = "$(digest "$repo")" ]; then
  ok "a stale writer lock nobody holds (naming a live PID) does not block --recover"
else bad "a stale writer lock blocked --recover (exit $st)"; printf '%s\n' "$out" >&2; fi
make_repo "$repo" 0.0.3
printf 'original lock\n' >"$repo/Cargo.lock"
before=$(digest "$repo")
killed_at after Cargo.toml --apply 0.0.4
rm -f "$work/held"
# Holds the lock, at most 20 s, from a process that is not a child of any run.
bash -c 'exec 9<>"$1" && flock -n 9 || exit 1; : >"$2"; for _ in $(seq 200); do sleep 0.1; done' \
  holder "$l" "$work/held" &
hpid=$!
for _ in $(seq 100); do [ -e "$work/held" ] && break; sleep 0.1; done
[ -e "$work/held" ] || bad "the lock holder never took the lock"
edited=$(digest "$repo")
out=$(run --recover); st=$?
if [ "$st" -eq 1 ] && [[ "$out" == *"still holds"* ]] && journal_left && [ "$edited" = "$(digest "$repo")" ]; then
  ok "a live holder of the writer lock makes --recover refuse and change nothing"
else bad "--recover ignored a live holder of the writer lock (exit $st)"; printf '%s\n' "$out" >&2; fi
kill -KILL "$hpid"; wait "$hpid" 2>/dev/null
settle
out=$(run --recover); st=$?
if [ "$st" -eq 0 ] && [ "$before" = "$(digest "$repo")" ]; then
  ok "once the holder exits, --recover restores the original fixture digest"
else bad "--recover after the holder exited failed (exit $st)"; printf '%s\n' "$out" >&2; fi

echo "== a successful apply leaves no backup or staged file behind"
make_repo "$repo" 0.0.3
run --apply 0.0.4 >/dev/null; st=$?
leftover=$(find "$repo" \( -name '*.bump.*' -o -name '.release-bump-journal*' \) | wc -l)
if [ "$st" -eq 0 ] && [ "$leftover" -eq 0 ]; then ok "apply removes its temporary and backup files"
else bad "apply left files behind (exit $st, $leftover)"; fi

echo "== a second --apply or --dry-run while an apply is still staging is refused"
make_repo "$repo" 0.0.3
printf 'original lock\n' >"$repo/Cargo.lock"
gate=$work/gate-a
rm -f "$gate.paused" "$gate.go"
SLASHIT_RELEASE_CARGO="$work/cargo-ok" PATH="$work/shim:$PATH" REAL_MV="$real_mv" CP_GATE="$gate" \
  bash "$src" --apply 0.0.4 "$repo" >"$work/run-a.out" 2>&1 &
pid_a=$!
for _ in $(seq 100); do [ -e "$gate.paused" ] && break; sleep 0.1; done
[ -e "$gate.paused" ] || bad "concurrency: the first apply never reached its staging copy"
out_b=$(run --apply 0.0.4); st_b=$?
out_d=$(run --dry-run 0.0.4); st_d=$?
: >"$gate.go"
wait "$pid_a"; st_a=$?
if [ "$st_a" -eq 0 ] && [ "$st_b" -eq 1 ] && [ "$st_d" -eq 1 ] && [[ "$out_d" == *"in progress"* ]] && [[ "$out_b" == *"holds"* ]] &&
  ! journal_left && grep -q '"version": "0.0.4"' "$repo/src-tauri/tauri.conf.json" &&
  grep -q '^version = "0.0.4"' "$repo/Cargo.toml" && ! find "$repo" -name '*.bump.*' | grep -q .; then
  ok "a second apply and a dry run are refused while the first stages; the first completes intact"
else
  bad "concurrent apply: first exit $st_a, second exit $st_b, dry run exit $st_d"
  printf 'second:\n%s\ndry run:\n%s\nfirst:\n%s\n' "$out_b" "$out_d" "$(cat "$work/run-a.out")" >&2
fi

echo "== no process started by the tests is left running"
for _ in $(seq 50); do pgrep -f -- "$work/" >/dev/null || break; sleep 0.1; done
if pgrep -f -- "$work/" >/dev/null; then bad "processes are still running: $(pgrep -f -- "$work/" | tr '\n' ' ')"
else ok "every stub, run and lock holder has exited"; fi

echo
echo "$pass passed, $fails failed"
[ "$fails" -eq 0 ]
