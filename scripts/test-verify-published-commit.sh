#!/usr/bin/env bash
# Self-test for scripts/verify-published-commit.sh.
#
# Puts a fake `gh` first on PATH that answers from environment variables, so
# no network, token or repository is involved, and asserts the exit status and
# message of each outcome.

set -uo pipefail
export LC_ALL=C

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)
tmp=$(mktemp -d "${TMPDIR:-/tmp}/verify-published-test.XXXXXX")
trap 'rm -rf "$tmp"' EXIT

cat >"$tmp/gh" <<'FAKE'
#!/usr/bin/env bash
# Answers `gh api <endpoint> --jq <expr>` from FAKE_* variables.
case $2 in
  */git/ref/heads/*) [ -n "${FAKE_HEAD-}" ] || { echo "Not Found" >&2; exit 1; }; echo "$FAKE_HEAD" ;;
  */commits/*) [ "${FAKE_COMMIT_API_FAILS-}" = 1 ] && { echo "boom" >&2; exit 1; }
               if [ -n "${FAKE_FILES-}" ]; then printf '%s\n' "$FAKE_FILES"; fi ;;
  *) exit 1 ;;
esac
FAKE
chmod +x "$tmp/gh"
export PATH="$tmp:$PATH"

script="$root/scripts/verify-published-commit.sh"
good=$(printf 'a%.0s' $(seq 1 40))
other=$(printf 'b%.0s' $(seq 1 40))
failures=0

# expect <name> <status> <output pattern> <args...>   (FAKE_* from the environment)
expect() {
  local name=$1 want=$2 pattern=$3 out status
  shift 3
  out=$("$script" "$@" 2>&1)
  status=$?
  if [ "$status" -ne "$want" ] || ! grep -Eq -- "$pattern" <<<"$out"; then
    echo "FAIL: $name (status $status, want $want; pattern '$pattern')"
    echo "$out" | sed 's/^/    /'
    failures=$((failures + 1))
  else
    echo "ok:   $name"
  fi
}

FAKE_HEAD=$good FAKE_FILES=$'src/a.rs\ndocs/b.md' \
  expect "published and changed" 0 "^OK: .* 2 file" o/r fix/x "$good"
FAKE_HEAD=$good FAKE_FILES=$'src/a.rs' \
  expect "path prefix touched" 0 "^OK" o/r fix/x "$good" --path src/
FAKE_HEAD=$other FAKE_FILES=$'src/a.rs' \
  expect "branch is at another commit" 1 "is at $other, not at $good" o/r fix/x "$good"
FAKE_HEAD="" \
  expect "branch missing" 1 "cannot read branch" o/r fix/x "$good"
FAKE_HEAD=$good FAKE_FILES="" \
  expect "empty commit" 1 "changes no files" o/r fix/x "$good"
FAKE_HEAD=$good FAKE_FILES=$'docs/b.md' \
  expect "path prefix untouched" 1 "nothing under 'src-tauri/'" o/r fix/x "$good" --path src-tauri/
FAKE_HEAD=$good FAKE_COMMIT_API_FAILS=1 \
  expect "commit API error is not a pass" 2 "cannot read commit" o/r fix/x "$good"
FAKE_HEAD=$good FAKE_FILES=x \
  expect "abbreviated commit refused" 2 "full 40-hex" o/r fix/x "aaaa"
FAKE_HEAD=$good FAKE_FILES=x \
  expect "missing arguments" 2 "usage" o/r

[ "$failures" -eq 0 ] && echo "all probes passed" || { echo "$failures probe(s) failed"; exit 1; }
