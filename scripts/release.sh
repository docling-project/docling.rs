#!/usr/bin/env bash
#
# Master-only release step (run by .github/workflows/ci.yml after the lint/test
# gates pass). Computes the next version from conventional commits; if there is
# one, it bumps the workspace version, commits + tags it, pushes to master,
# publishes the changed crates, and assembles the GitHub Release notes (a commit
# list since the previous tag) for the workflow to publish. A clean no-op when no
# release-worthy commit landed since the last tag.
#
# The release commit is pushed with RELEASE_PAT (an admin token, so it satisfies
# the master branch ruleset) and carries `[skip ci]`, so it does not re-trigger CI.
#
# Set FORCE_VERSION=X.Y.Z to (re)publish that exact version instead of computing it
# from the commit history — used to release a version a failed/blocked run skipped.
#
# Requires: CARGO_REGISTRY_TOKEN (publish) and push access to master.
# Usage: scripts/release.sh
set -euo pipefail
cd "$(dirname "$0")/.."

new="${FORCE_VERSION:-$(scripts/bump_version.sh)}"
if [[ -z "$new" ]]; then
  echo "No release-worthy commits since the last tag — nothing to release."
  exit 0
fi
[[ -n "${FORCE_VERSION:-}" ]] && echo ">> forced release of v$new"

current="$(grep -m1 '^version = ' Cargo.toml | sed -E 's/.*"([^"]+)".*/\1/')"
echo ">> releasing v$new (was v$current)"

# Assemble the GitHub Release notes BEFORE the version-bump commit lands, so the
# range is exactly the feature commits going into this release (everything since
# the previous tag, the release chore commit excluded). The workflow turns this
# into the Release description.
prev_tag="$(git tag --list 'v*' --sort=-version:refname | head -n1)"
notes_range="${prev_tag:+$prev_tag..}HEAD"
notes_file="$(pwd)/release-notes.md"
repo="${GITHUB_REPOSITORY:-artiz/docling.rs}"
{
  echo "## What's changed"
  echo
  git log "$notes_range" --no-merges --format='- %s (%h)'
  if [[ -n "$prev_tag" ]]; then
    echo
    echo "**Full changelog**: https://github.com/$repo/compare/$prev_tag...v$new"
  fi
} >"$notes_file"

# Bump the single version key in the root manifest's [workspace.package].
sed -i -E "0,/^version = \"[^\"]+\"/ s//version = \"$new\"/" Cargo.toml

# Keep internal path-dependency requirements in lockstep with the workspace
# version. Without this the published crates resolve each other to an OLDER
# release (the version req is a literal, not inherited), which fails to build.
for manifest in crates/*/Cargo.toml; do
  sed -i -E "/path = \"\.\.\/docling/ s/version = \"[^\"]+\"/version = \"$new\"/" "$manifest"
done

git config user.name "github-actions[bot]"
git config user.email "41898282+github-actions[bot]@users.noreply.github.com"
git add Cargo.toml crates/*/Cargo.toml
# Commit only if the bump changed something — a forced re-publish of a version
# whose manifests are already at $new has nothing to commit.
if git diff --cached --quiet; then
  echo ">> manifests already at v$new — re-tagging / re-publishing only"
else
  git commit -m "chore(release): v$new [skip ci]"
fi
# Tag only if it doesn't already exist (idempotent for a forced re-publish).
if ! git rev-parse -q --verify "refs/tags/v$new" >/dev/null; then
  git tag -a "v$new" -m "v$new"
fi
git push origin HEAD:master
git push origin "v$new"

# Publish every crate at the new version (idempotent: skips any already on
# crates.io), in dependency order.
scripts/ci_publish.sh

# Hand the released version + notes file to the workflow, which cuts the GitHub
# Release for the tag we just pushed. Guarded so the script still runs locally
# (where $GITHUB_OUTPUT is unset).
if [[ -n "${GITHUB_OUTPUT:-}" ]]; then
  {
    echo "released=true"
    echo "version=$new"
    echo "notes_file=$notes_file"
  } >>"$GITHUB_OUTPUT"
fi

echo ">> released v$new"
