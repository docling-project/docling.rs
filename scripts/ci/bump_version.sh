#!/usr/bin/env bash
#
# Decide the next workspace version from the conventional-commit messages since
# the last release tag, and print it to stdout. Prints NOTHING (exit 0) when no
# release-worthy commit is found, so the caller can skip the release.
#
#   <type>!: …  or  "BREAKING CHANGE" in the body  -> major
#   feat: …                                        -> minor
#   fix:/perf:/revert: …                           -> patch
#   docs/chore/ci/refactor/test/style/build/…      -> no release
#
# Pure: reads git history + the root Cargo.toml; writes nothing.
# Usage: scripts/ci/bump_version.sh
set -euo pipefail
cd "$(dirname "$0")/../.."

current="$(grep -m1 '^version = ' Cargo.toml | sed -E 's/.*"([^"]+)".*/\1/')"

last_tag="$(git tag --list 'v*' --sort=-version:refname | head -n1)"
if [[ -n "$last_tag" ]]; then
  range="$last_tag..HEAD"
else
  range="HEAD" # no release tag yet: consider the whole history
fi

# Subject + body of every non-merge commit in range.
log="$(git log "$range" --no-merges --format='%s%n%b')"

bump=""
if grep -qE '^[a-z]+(\([^)]*\))?!:' <<<"$log" || grep -q 'BREAKING CHANGE' <<<"$log"; then
  bump="major"
elif grep -qE '^feat(\([^)]*\))?:' <<<"$log"; then
  bump="minor"
elif grep -qE '^(fix|perf|revert)(\([^)]*\))?:' <<<"$log"; then
  bump="patch"
fi

[[ -z "$bump" ]] && exit 0

IFS=. read -r major minor patch <<<"$current"
case "$bump" in
major)
  major=$((major + 1))
  minor=0
  patch=0
  ;;
minor)
  minor=$((minor + 1))
  patch=0
  ;;
patch) patch=$((patch + 1)) ;;
esac
echo "$major.$minor.$patch"
