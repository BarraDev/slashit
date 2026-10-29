#!/usr/bin/env bash
# SPDX-License-Identifier: MPL-2.0
# Refuses a release tag unless the commit it names is reachable from main.
#
# The release workflow signs with the production updater key, so it may sign only
# a commit that main has accepted. A tag can point at any commit, and an annotated
# tag is a different object from the commit it names. This script therefore
# resolves the tag to its commit (`^{commit}`), requires the checkout to be that
# commit, and then asks git whether that commit is an ancestor of origin/main.
#
# It does not read GITHUB_SHA. For an annotated tag that value may be the tag
# object's SHA rather than the commit's.
#
# Run it from a checkout with full history (fetch-depth: 0) in which the tag ref
# exists. It fails closed: any error stops the signing job.
#
# usage: scripts/check-release-tag-on-main.sh <tag>
# Exit status: 0 the tag names a commit on origin/main. 1 refused. 2 input or
# fetch error.
set -euo pipefail

if [ "$#" -ne 1 ]; then
  echo "usage: scripts/check-release-tag-on-main.sh <tag>" >&2
  exit 2
fi
tag=$1

if ! tag_commit=$(git rev-parse --verify --quiet "refs/tags/${tag}^{commit}"); then
  echo "INPUT ERROR: refs/tags/${tag} does not exist or does not name a commit" >&2
  exit 2
fi

head_commit=$(git rev-parse --verify HEAD)
if [ "$tag_commit" != "$head_commit" ]; then
  echo "FAIL the checkout is at $head_commit, but tag $tag names $tag_commit; refusing to sign a different commit" >&2
  exit 1
fi

if ! git fetch --quiet --no-tags origin '+refs/heads/main:refs/remotes/origin/main'; then
  echo "INPUT ERROR: could not fetch origin/main; refusing to sign without it" >&2
  exit 2
fi

if ! git merge-base --is-ancestor "$tag_commit" refs/remotes/origin/main; then
  echo "FAIL tag $tag names $tag_commit, which is not reachable from origin/main; refusing to sign" >&2
  exit 1
fi

echo "ok   tag $tag names $tag_commit, which is reachable from origin/main"
