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
#      Every file is staged beside its destination first; the staged binary is
#      the one checked for embedding and hashed, and its hash is checked again
#      immediately before the rename, so what is published is what was verified;
#   5. write the sidecar target/dogfood/slashit-ui.frontend last, binding the
#      declaration "embedded" to the promoted binary's sha256.
#
# One build at a time per artifact root: an flock on target/dogfood/.build.lock,
# held from before the source is read until publication ends and released by
# the kernel when this process dies. A second build is refused, not queued, so
# two workspaces can never interleave a build and a publication.
#
# Source identity: a build is promoted only when the source it was built from
# has a real commit identity, read before and after the build.
#   - Git checkout (a `.git` exists, colocated JJ included): `git rev-parse HEAD`
#     (a detached HEAD is fine; a branch name is never an identity), plus a
#     digest of the status, the diff against HEAD and every untracked file.
#   - JJ-only workspace (`.jj` without `.git`): the commit id of `@` after JJ's
#     working-copy snapshot, with the change id recorded separately. The snapshot
#     is content-addressed, so the commit id covers uncommitted edits too; `dirty`
#     means `@` is not empty.
#   - Anything else, an unborn HEAD, a conflicted `@` or a failing VCS command:
#     the build is refused before it starts. There is no HEAD or `unknown` fallback.
# Known gap, accepted: comparing identity before and after cannot see a source
# edit that is made during the build and reverted before it ends. Closing it would
# mean building from an immutable export in a scratch directory, which gives up
# the incremental Cargo/Trunk caches (a cold debug Tauri build per promotion) and
# moves every path the toolchain sees; the edit-and-revert window is narrow and
# needs a writer in the same workspace, so it is documented rather than removed.
# Files that Git or JJ ignore, and files JJ declines to snapshot (over
# snapshot.max-new-file-size, or with auto-track off), are not covered.
#
# The sidecar is the declared frontend mode scripts/supervise-dogfood.sh reads
# (mode, sha256, commit). A binary without one is a development build and needs
# the dev server.
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
# promotes where the supervisor looks. A linked JJ workspace has no .git; its
# .jj/repo file points into the primary workspace, which is the shared root, so
# every workspace of one repository contends on one lock and one target/dogfood.
# A directory that is neither is refused below with the source identity.
promoted=$root/target/dogfood
if [ -e "$root/.git" ]; then
  common=$(git -C "$root" rev-parse --path-format=absolute --git-common-dir) ||
    fail "cannot resolve the git common directory of $root"
  [ "$(basename "$common")" = .git ] && promoted=$(dirname "$common")/target/dogfood
elif [ -f "$root/.jj/repo" ]; then
  jj_repo=$(cd "$root/.jj" && cd "$(cat repo)" && pwd -P) ||
    fail "cannot resolve the primary JJ workspace from $root/.jj/repo"
  primary=$(dirname "$(dirname "$jj_repo")")
  [ -d "$primary/.jj/repo" ] || fail "$jj_repo is not the repository of a JJ workspace"
  promoted=$primary/target/dogfood
fi
binary=$promoted/slashit-ui
cli=$promoted/slashit
sidecar=$binary.frontend

mkdir -p "$promoted" || fail "cannot create $promoted"
exec 9>"$promoted/.build.lock" || fail "cannot open $promoted/.build.lock"
flock -n 9 || fail "another dogfood build holds $promoted/.build.lock; nothing is built or promoted"

grep -Eq '"beforeBuildCommand": *"trunk build"' src-tauri/tauri.conf.json ||
  fail "src-tauri/tauri.conf.json no longer runs \`trunk build\`; update this script to match"

# Provenance is read before the build: the sidecar names the source the binary
# was built from. The same reads after the build must agree, or nothing is promoted.
# Prints key=value lines (vcs, commit, change, dirty, changed, digest) or fails
# with the reason on stderr.
git_identity() {
  local head status diff untracked
  head=$(git -C "$root" rev-parse --verify -q 'HEAD^{commit}') || { echo "HEAD is not a commit (unborn branch?)" >&2; return 1; }
  status=$(git -C "$root" status --porcelain) || return 1
  diff=$(git -C "$root" diff --binary HEAD | sha256sum) || return 1
  untracked=$(cd "$root" && git ls-files -o --exclude-standard -z | xargs -0 -r sha256sum --) || return 1
  printf 'vcs=git\ncommit=%s\nchange=\ndirty=%s\nchanged=%s\ndigest=%s\n' "$head" \
    "$([ -n "$status" ] && echo yes || echo no)" "$(printf '%s' "$status" | tr '\n' ';')" \
    "$(printf '%s\n%s\n%s\n%s\n' "$head" "$status" "$diff" "$untracked" | sha256sum | cut -d' ' -f1)"
}
jj_identity() {
  local jj=(jj) line commit change state changed
  # The project pins jj in mise.toml; prefer it, as scripts/check-agent-docs.sh does.
  if command -v mise >/dev/null 2>&1 && [ -f "$root/mise.toml" ]; then jj=(mise exec -- jj); fi
  # Every jj command snapshots the working copy first, so this reads the files as they are on disk.
  line=$("${jj[@]}" --no-pager --color=never log -r @ --no-graph \
    -T 'commit_id ++ " " ++ change_id ++ " " ++ if(conflict, "conflict", if(empty, "clean", "dirty"))') ||
    { echo "jj could not read the working-copy commit" >&2; return 1; }
  read -r commit change state <<<"$line"
  [ "$state" != conflict ] || { echo "the working-copy commit has conflicts" >&2; return 1; }
  changed=$("${jj[@]}" --no-pager --color=never diff --summary -r @) || return 1
  printf 'vcs=jj\ncommit=%s\nchange=%s\ndirty=%s\nchanged=%s\ndigest=%s\n' "$commit" "$change" \
    "$([ "$state" = dirty ] && echo yes || echo no)" "$(printf '%s' "$changed" | tr '\n' ';')" \
    "$(printf 'jj\n%s\n' "$commit" | sha256sum | cut -d' ' -f1)"
}
source_identity() {
  local id commit
  if [ -e "$root/.git" ]; then id=$(git_identity) || return 1
  elif [ -d "$root/.jj" ]; then id=$(jj_identity) || return 1
  else echo "$root is neither a Git checkout nor a JJ workspace" >&2; return 1; fi
  commit=$(sed -n 's/^commit=//p' <<<"$id")
  [[ $commit =~ ^[0-9a-f]{40}([0-9a-f]{24})?$ ]] || { echo "not a commit id: '$commit'" >&2; return 1; }
  printf '%s\n' "$id"
}
id_field() { sed -n "s/^$2=//p" <<<"$1" | head -n 1; }
src_id=$(source_identity) ||
  fail "cannot establish the source identity of $root (see above); nothing is built or promoted"
src_commit=$(id_field "$src_id" commit)

echo "==> trunk build (release-style frontend into dist/)"
trunk build 9>&- || fail "trunk build failed; see above"

echo "==> cargo tauri build --debug --no-bundle (assets embedded)"
cargo tauri build --debug --no-bundle \
  --config '{"build":{"beforeBuildCommand":null}}' 9>&- ||
  fail "cargo tauri build failed; see above"

echo "==> cargo build -p slashit (the CLI the supervisor verifies against)"
cargo build -p slashit 9>&- || fail "cargo build -p slashit failed; see above"

wasm=$(find dist -maxdepth 1 -name '*_bg.wasm' -printf '%f\n' | head -n 1)
[ -n "$wasm" ] || fail "no *_bg.wasm in dist/"
[ -x "$cargo_cli" ] || fail "$cargo_cli is missing after the build"
end_id=$(source_identity) ||
  fail "cannot re-establish the source identity after the build (see above); nothing is promoted, the previous build is untouched"
[ "$end_id" = "$src_id" ] ||
  fail "the source changed during the build (commit $src_commit before, $(id_field "$end_id" commit) after; changed before: [$(id_field "$src_id" changed)], after: [$(id_field "$end_id" changed)]; digest $(id_field "$src_id" digest) before, $(id_field "$end_id" digest) after); nothing is promoted, the previous build is untouched"

# Promotion: copy to a temporary name beside the destination, then rename. A
# rename replaces the directory entry and leaves a running executable intact.
# Nothing is removed before the new pair is complete: the previous pair is kept
# aside, and any failure from here on restores it, so the last known-good
# binary and its sidecar stay consistent.
# Temporary names come from mktemp, not from $$: a predictable name in a shared
# directory can be pre-created as a symlink and written through.
backup=$(mktemp -d "$promoted/.previous.XXXXXX") ||
  fail "cannot create a backup directory in $promoted"
for f in "$binary" "$cli" "$sidecar"; do
  [ -f "$f" ] && cp -p -- "$f" "$backup/${f##*/}"
done
promoted_ok=0
staged=()
restore() {
  rm -f -- "${staged[@]}"
  [ "$promoted_ok" = 1 ] && return
  for f in "$binary" "$cli" "$sidecar"; do
    if [ -f "$backup/${f##*/}" ]; then mv -f -- "$backup/${f##*/}" "$f"
    elif [ -f "$f" ]; then rm -f -- "$f"; fi
  done
  rm -rf -- "$backup"
  echo "build-dogfood-app: promotion failed; the previous promoted pair is restored" >&2
}
trap restore EXIT
stage() { # source destination: prints the staged path
  local tmp
  tmp=$(mktemp "$2.tmp.XXXXXX") || fail "cannot create a temporary file beside $2"
  cp -- "$1" "$tmp" && chmod 755 "$tmp" || { rm -f -- "$tmp"; fail "cannot stage $1 beside $2"; }
  echo "$tmp"
}
publish() { # staged destination expected-sha256
  [ "$(sha256sum "$1" | cut -d' ' -f1)" = "$3" ] ||
    fail "$1 changed after it was verified; nothing more is published"
  mv -f -- "$1" "$2" || fail "cannot promote $1 to $2"
}
bin_tmp=$(stage "$cargo_binary" "$binary"); staged+=("$bin_tmp")
cli_tmp=$(stage "$cargo_cli" "$cli"); staged+=("$cli_tmp")
# Validation runs on the staged copies, the files that will be renamed.
grep -a -q -F "$wasm" "$bin_tmp" ||
  fail "$cargo_binary does not contain $wasm: the frontend is not embedded (a development build?); nothing is promoted"
sha=$(sha256sum "$bin_tmp" | cut -d' ' -f1)
cli_sha=$(sha256sum "$cli_tmp" | cut -d' ' -f1)

side_tmp=$(mktemp "$sidecar.tmp.XXXXXX") ||
  fail "cannot create a temporary file beside $sidecar"
staged+=("$side_tmp")
# Provenance: the source commit (and JJ change id), whether the tree had
# uncommitted changes, and the digest of the source. The hashes below are of the
# staged files, the ones renamed into place. The supervisor reads only the keys
# it knows; the rest are for the refresh command and for a human reading the file.
{
  echo "mode=embedded"
  echo "sha256=$sha"
  echo "wasm=$wasm"
  echo "vcs=$(id_field "$src_id" vcs)"
  echo "commit=$src_commit"
  [ -z "$(id_field "$src_id" change)" ] || echo "change=$(id_field "$src_id" change)"
  echo "dirty=$(id_field "$src_id" dirty)"
  echo "source_digest=$(id_field "$src_id" digest)"
  echo "cli_sha256=$cli_sha"
} >"$side_tmp" && chmod 644 "$side_tmp" ||
  fail "cannot write the sidecar $sidecar"

# Each staged file's hash is checked again immediately before its rename; the
# sidecar goes last, so it never vouches for a binary that was not published.
publish "$bin_tmp" "$binary" "$sha"
publish "$cli_tmp" "$cli" "$cli_sha"
mv -f -- "$side_tmp" "$sidecar" || fail "cannot write the sidecar $sidecar"
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
