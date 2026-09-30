#!/usr/bin/env bash
# Prove crates/slashit-acceptance/shard-manifest.txt exactly covers the
# product acceptance journeys: every test in exactly one shard, no stale
# entries. Needs only a compile of the test binary -- no display, no built
# application, no WebDriver.
set -euo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$root"

manifest=crates/slashit-acceptance/shard-manifest.txt

fail() {
  echo "check-acceptance-shards: $*" >&2
  exit 1
}

[[ -f $manifest ]] || fail "$manifest is missing"

actual=$(cargo test -q -p slashit-acceptance --features run-acceptance --test product_acceptance -- --list |
  sed -n 's/: test$//p' | sort)
[[ -n $actual ]] || fail "\`cargo test --list\` reported no product acceptance tests; something upstream is broken"

manifest_names=$(grep -v '^[[:space:]]*#' "$manifest" | grep -v '^[[:space:]]*$' | cut -f2 | sort)
[[ -n $manifest_names ]] || fail "$manifest lists no tests"

status=0

dupes=$(echo "$manifest_names" | uniq -d)
if [[ -n $dupes ]]; then
  echo "check-acceptance-shards: listed in more than one shard (or twice in the same shard):" >&2
  echo "$dupes" >&2
  status=1
fi

missing=$(comm -23 <(echo "$actual") <(echo "$manifest_names" | sort -u))
if [[ -n $missing ]]; then
  echo "check-acceptance-shards: not assigned to any shard; add each to exactly one line in $manifest:" >&2
  echo "$missing" >&2
  status=1
fi

stale=$(comm -13 <(echo "$actual") <(echo "$manifest_names" | sort -u))
if [[ -n $stale ]]; then
  echo "check-acceptance-shards: $manifest names tests that no longer exist (removed or renamed); delete or fix these lines:" >&2
  echo "$stale" >&2
  status=1
fi

((status == 0)) && echo "check-acceptance-shards: $manifest exactly covers all $(echo "$actual" | wc -l) product acceptance tests"
exit $status
