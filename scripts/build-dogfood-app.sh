#!/usr/bin/env bash
# SPDX-License-Identifier: MPL-2.0
# Build the self-contained dogfood binary: the frontend (HTML, CSS, JS, WASM)
# is embedded at compile time, so the GUI needs no Trunk process and no
# server on localhost:1420, now or after a reboot. Development keeps its own
# loop: `trunk serve` / `./dev.sh` are untouched and nothing here edits a
# configuration file.
#
#   1. `trunk build`: the same command tauri.conf.json's beforeBuildCommand
#      runs, executed once here so the build step below can skip it;
#   2. `cargo tauri build --debug --no-bundle` with embedding: the Tauri CLI
#      enables the custom-protocol feature, which is what makes the binary load
#      assets from the bundle instead of build.devUrl;
#   3. prove the embedding: the WASM file name that dist/index.html loads must
#      appear inside the binary, otherwise the build is refused;
#   4. promote the binary and its `slashit` CLI into target/dogfood/. Cargo's
#      target/debug is shared with scripts/build-acceptance-app.sh, which
#      overwrites slashit-ui with a binary that has no embedded frontend. A
#      promoted copy keeps the dogfood sidecar's sha256 valid across acceptance
#      builds. Each file is published by rename, so a running GUI's executable
#      is never truncated in place;
#   5. write the sidecar target/dogfood/slashit-ui.frontend last, binding the
#      declaration "embedded" to the promoted binary's sha256.
#
# The sidecar is the declared frontend mode scripts/supervise-dogfood.sh reads.
# A binary without one is a development build and needs the dev server.
set -euo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$root"

fail() {
  echo "build-dogfood-app: $*" >&2
  exit 1
}

cargo_binary=target/debug/slashit-ui
cargo_cli=target/debug/slashit
# The artifact root is the one scripts/supervise-dogfood.sh launches from: the
# checkout that owns the common .git directory, so a build in a linked worktree
# promotes where the supervisor looks. Without git, this checkout.
common=$(git -C "$root" rev-parse --path-format=absolute --git-common-dir 2>/dev/null) || common=
if [ -n "$common" ] && [ "$(basename "$common")" = .git ]; then
  promoted=$(dirname "$common")/target/dogfood
else
  promoted=$root/target/dogfood
fi
binary=$promoted/slashit-ui
cli=$promoted/slashit
sidecar=$binary.frontend

grep -Eq '"beforeBuildCommand": *"trunk build"' src-tauri/tauri.conf.json ||
  fail "src-tauri/tauri.conf.json no longer runs \`trunk build\`; update this script to match"

# Provenance is read before the build: the sidecar names the source the binary
# was built from. The same reads after the build must agree, or nothing is promoted.
src_commit=$(git -C "$root" rev-parse HEAD 2>/dev/null || echo unknown)
src_status=$(git -C "$root" status --porcelain 2>/dev/null || true)

echo "==> trunk build (release-style frontend into dist/)"
trunk build || fail "trunk build failed; see above"

echo "==> cargo tauri build --debug --no-bundle (assets embedded)"
cargo tauri build --debug --no-bundle \
  --config '{"build":{"beforeBuildCommand":null}}' ||
  fail "cargo tauri build failed; see above"

echo "==> cargo build -p slashit (the CLI the supervisor verifies against)"
cargo build -p slashit || fail "cargo build -p slashit failed; see above"

wasm=$(find dist -maxdepth 1 -name '*_bg.wasm' -printf '%f\n' | head -n 1)
[ -n "$wasm" ] || fail "no *_bg.wasm in dist/"
grep -a -q -F "$wasm" "$cargo_binary" ||
  fail "$cargo_binary does not contain $wasm: the frontend is not embedded (a development build?); nothing is promoted"
[ -x "$cargo_cli" ] || fail "$cargo_cli is missing after the build"
end_commit=$(git -C "$root" rev-parse HEAD 2>/dev/null || echo unknown)
end_status=$(git -C "$root" status --porcelain 2>/dev/null || true)
[ "$end_commit" = "$src_commit" ] && [ "$end_status" = "$src_status" ] ||
  fail "the source changed during the build (HEAD $src_commit before, $end_commit after; status before: [$src_status], after: [$end_status]); nothing is promoted, the previous build is untouched"

# Promotion: copy to a temporary name beside the destination, then rename. A
# rename replaces the directory entry and leaves a running executable intact.
# Nothing is removed before the new pair is complete: the previous pair is kept
# aside, and any failure from here on restores it, so the last known-good
# binary and its sidecar stay consistent.
mkdir -p "$promoted"
# Temporary names come from mktemp, not from $$: a predictable name in a shared
# directory can be pre-created as a symlink and written through.
backup=$(mktemp -d "$promoted/.previous.XXXXXX") ||
  fail "cannot create a backup directory in $promoted"
for f in "$binary" "$cli" "$sidecar"; do
  [ -f "$f" ] && cp -p -- "$f" "$backup/${f##*/}"
done
promoted_ok=0
restore() {
  [ "$promoted_ok" = 1 ] && return
  for f in "$binary" "$cli" "$sidecar"; do
    if [ -f "$backup/${f##*/}" ]; then mv -f -- "$backup/${f##*/}" "$f"
    elif [ -f "$f" ]; then rm -f -- "$f"; fi
  done
  rm -rf -- "$backup"
  echo "build-dogfood-app: promotion failed; the previous promoted pair is restored" >&2
}
trap restore EXIT
publish() { # source destination
  local tmp
  tmp=$(mktemp "$2.tmp.XXXXXX") || fail "cannot create a temporary file beside $2"
  cp -- "$1" "$tmp" && chmod 755 "$tmp" && mv -f -- "$tmp" "$2" ||
    { rm -f -- "$tmp"; fail "cannot promote $1 to $2"; }
}
publish "$cargo_binary" "$binary"
publish "$cargo_cli" "$cli"

# The sidecar is written beside its binary through a temporary file and a
# rename, so the supervisor never reads a half-written declaration.
sha=$(sha256sum "$binary" | cut -d' ' -f1)
side_tmp=$(mktemp "$sidecar.tmp.XXXXXX") ||
  fail "cannot create a temporary file beside $sidecar"
# Provenance: the source commit this binary was built from, and whether the
# working tree had uncommitted changes. The supervisor reads only the keys it
# knows; these two are for the refresh command and for a human reading the file.
commit=$src_commit
dirty=no
[ -n "$src_status" ] && dirty=yes
{
  echo "mode=embedded"
  echo "sha256=$sha"
  echo "wasm=$wasm"
  echo "commit=$commit"
  echo "dirty=$dirty"
} >"$side_tmp" && chmod 644 "$side_tmp" && mv -f -- "$side_tmp" "$sidecar" ||
  { rm -f -- "$side_tmp"; fail "cannot write the sidecar $sidecar"; }
promoted_ok=1

# Keep the replaced pair as the single rollback candidate in $promoted/previous,
# but only when it is self-consistent: its sidecar's sha256 must name its binary.
# An inconsistent or absent previous pair is not kept. The directory is replaced
# on each promotion, never accumulated, because a debug binary is about 180 MB.
old_sha=$(sed -n 's/^sha256=//p' "$backup/${sidecar##*/}" 2>/dev/null | head -n 1) || old_sha=
old_bin=$(sha256sum "$backup/${binary##*/}" 2>/dev/null | cut -d' ' -f1) || old_bin=
if [ -n "$old_sha" ] && [ "$old_sha" = "$old_bin" ] &&
  rm -rf -- "$promoted/previous" && mv -- "$backup" "$promoted/previous" 2>/dev/null; then
  echo "==> previous verified pair kept in $promoted/previous (rollback candidate)"
else
  rm -rf -- "$backup"
  echo "==> no verified previous pair to keep"
fi
echo "==> embedded frontend verified ($wasm); promoted to $binary and recorded in $sidecar"
