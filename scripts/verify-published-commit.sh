#!/usr/bin/env bash
# Verify that a branch on GitHub publishes the commit you meant to publish,
# and that this commit really changes files.
#
#   scripts/verify-published-commit.sh <owner/repo> <branch> <commit> [--path <prefix>]...
#
# <commit> is the full 40-hex commit ID you validated locally (a JJ commit ID
# is the Git commit ID). It reads only the GitHub API through `gh`, so it works
# from a JJ workspace, which has no .git, and it never trusts the output of the
# push that was just run. It fails when:
#
#   - the branch does not exist, or its head is not exactly <commit>;
#   - <commit> changes no files (an empty "correction"); or
#   - a given --path prefix is touched by none of its changed files.
#
# --path prefixes must be non-empty (an empty prefix matches every file). The
# changed files are read across all pages, but GitHub lists at most 3000 files
# per commit: when the list reaches that cap, a --path that is not found is
# reported as unprovable (exit 2) instead of absent, and an OK is labelled as
# partial coverage.
#
# It proves what GitHub holds, not that hosted checks passed or that the code
# is right. Run it after the push and before reporting a fix as published.
#
# Exit status: 0 verified, 1 verification failed, 2 usage or API error.

set -uo pipefail
export LC_ALL=C

usage() {
  echo "usage: ${0##*/} <owner/repo> <branch> <40-hex-commit> [--path <prefix>]..." >&2
  exit 2
}

[ "$#" -ge 3 ] || usage
repo=$1 branch=$2 commit=$3
shift 3
paths=()
while [ "$#" -gt 0 ]; do
  [ "$1" = "--path" ] && [ "$#" -ge 2 ] || usage
  [ -n "$2" ] || { echo "error: --path needs a non-empty prefix" >&2; exit 2; }
  paths+=("$2")
  shift 2
done

case $commit in
  *[!0-9a-f]* | "") echo "error: <commit> must be lowercase hex" >&2; exit 2 ;;
esac
[ "${#commit}" -eq 40 ] || { echo "error: <commit> must be the full 40-hex commit ID" >&2; exit 2; }

# Capture, then test the status: a pipeline would hide a failing `gh`.
if ! head=$(gh api "repos/$repo/git/ref/heads/$branch" --jq .object.sha 2>&1); then
  echo "FAIL: cannot read branch '$branch' of $repo: $head" >&2
  exit 1
fi
if [ "$head" != "$commit" ]; then
  echo "FAIL: $repo '$branch' is at $head, not at $commit" >&2
  exit 1
fi

max_files=3000 # GitHub's cap on the files it lists for one commit
if ! files=$(gh api "repos/$repo/commits/$commit" --paginate --jq '.files[].filename' 2>&1); then
  echo "FAIL: cannot read commit $commit of $repo: $files" >&2
  exit 2
fi
if [ -z "$files" ]; then
  echo "FAIL: $commit changes no files; an empty commit is not a code fix" >&2
  exit 1
fi

count=$(printf '%s\n' "$files" | wc -l)
capped=0
[ "$count" -ge "$max_files" ] && capped=1

for prefix in "${paths[@]}"; do
  hit=0
  while IFS= read -r f; do
    case $f in "$prefix"*) hit=1; break ;; esac
  done <<<"$files"
  if [ "$hit" -eq 0 ] && [ "$capped" -eq 1 ]; then
    echo "FAIL: $commit lists $count files, GitHub's limit; cannot prove nothing under '$prefix' changed" >&2
    exit 2
  fi
  if [ "$hit" -eq 0 ]; then
    echo "FAIL: $commit changes nothing under '$prefix'" >&2
    exit 1
  fi
done

note=""
[ "$capped" -eq 1 ] && note=" (GitHub lists at most $max_files files; coverage of the commit is partial)"
echo "OK: $repo '$branch' is at $commit, which changes $count file(s)$note"
