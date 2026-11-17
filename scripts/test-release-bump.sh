#!/usr/bin/env bash
# SPDX-License-Identifier: MPL-2.0
# Hermetic test of scripts/release-bump.sh in disposable fixture repositories.
# Cargo is a stub (SLASHIT_RELEASE_CARGO, kept so each run can pick its own stub),
# so nothing is compiled and no lockfile is rewritten by a real resolver. A rename
# failure is injected with a test-only mv shim first on PATH.
#
# usage: scripts/test-release-bump.sh
set -uo pipefail
export LC_ALL=C

src=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/release-bump.sh
work=$(mktemp -d "${TMPDIR:-/tmp}/release-bump-test.XXXXXX") || { echo "cannot create a temporary directory" >&2; exit 1; }
# Every rm below runs on a path under $work, so $work must be a real directory.
case $work in /?*/?*) [ -d "$work" ] || { echo "temporary directory $work is missing" >&2; exit 1; } ;;
  *) echo "refusing to use '$work' as the temporary directory" >&2; exit 1 ;; esac
trap 'rm -rf -- "$work"' EXIT
real_mv=$(command -v mv)
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
# Announces itself, then sleeps until killed.
cat >"$work/cargo-sleep" <<STUB
#!/usr/bin/env bash
echo "written by cargo" >Cargo.lock
: >"$work/started"
exec sleep 30
STUB
# Kills its parent (the bump script) outright, so no trap runs.
cat >"$work/cargo-crash" <<'STUB'
#!/usr/bin/env bash
echo "written by cargo" >Cargo.lock
kill -9 "$PPID"
STUB
chmod +x "$work"/cargo-*
# A mv that fails only when the destination is $MV_FAIL_TARGET.
mkdir "$work/shim"
cat >"$work/shim/mv" <<'STUB'
#!/usr/bin/env bash
for last; do :; done
[ "$last" = "$MV_FAIL_TARGET" ] && exit 1
exec "$REAL_MV" "$@"
STUB
chmod +x "$work/shim/mv"

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
  (cd "$1" && find . -type f -print0 | sort -z | xargs -0 sha256sum)
}

repo=$work/repo
make_repo "$repo" 0.0.3
run() { # args...: run the script in the fixture with the stub cargo
  SLASHIT_RELEASE_CARGO=${CARGO_STUB:-$work/cargo-ok} bash "$src" "$@" "$repo" 2>&1
}

echo "== dry run is read-only and reports the bump"
before=$(digest "$repo")
out=$(run --dry-run 0.0.4); st=$?
if [ "$st" -eq 0 ] && [[ "$out" == *"0.0.3 -> 0.0.4"* ]] && [ "$before" = "$(digest "$repo")" ]; then
  ok "--dry-run reports the bump and changes nothing"
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
for sig in TERM HUP; do
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

echo "== a crashed run leaves a journal that blocks the next run and --recover clears"
make_repo "$repo" 0.0.3
printf 'original lock\n' >"$repo/Cargo.lock"
before=$(digest "$repo")
CARGO_STUB="$work/cargo-crash" run --apply 0.0.4 >/dev/null
[ -d "$repo/.release-bump-journal" ] && [ "$before" != "$(digest "$repo")" ] &&
  ok "a SIGKILLed run leaves the journal and changed files" || bad "the crash did not leave a journal"
crashed=$(digest "$repo")
out=$(run --apply 0.0.5); st=$?
if [ "$st" -eq 1 ] && [[ "$out" == *"--recover"* ]] && [ "$crashed" = "$(digest "$repo")" ]; then
  ok "a later apply refuses and points to --recover without touching anything"
else bad "apply was not blocked by the journal (exit $st)"; printf '%s\n' "$out" >&2; fi
out=$(run --dry-run 0.0.4); st=$?
[ "$st" -eq 1 ] && ok "so does --dry-run" || bad "--dry-run ignored the journal (exit $st)"
out=$(run --recover); st=$?
if [ "$st" -eq 0 ] && [ "$before" = "$(digest "$repo")" ]; then
  ok "--recover restores the original fixture digest and removes the journal"
else bad "--recover did not restore (exit $st)"; printf '%s\n' "$out" >&2; fi
out=$(run --recover); st=$?
[ "$st" -eq 1 ] && ok "--recover with no journal refuses" || bad "--recover without a journal returned $st"

echo "== a successful apply leaves no backup or staged file behind"
make_repo "$repo" 0.0.3
run --apply 0.0.4 >/dev/null; st=$?
leftover=$(find "$repo" \( -name '*.bump.*' -o -name '.release-bump-journal*' \) | wc -l)
if [ "$st" -eq 0 ] && [ "$leftover" -eq 0 ]; then ok "apply removes its temporary and backup files"
else bad "apply left files behind (exit $st, $leftover)"; fi

echo
echo "$pass passed, $fails failed"
[ "$fails" -eq 0 ]
