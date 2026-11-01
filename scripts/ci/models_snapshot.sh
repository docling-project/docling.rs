#!/usr/bin/env bash
# Freeze a models release into an immutable, content-addressed copy (#951).
#
#   scripts/ci/models_snapshot.sh [models-v1]    # GITHUB_REPOSITORY or docling-project/docling.rs
#
# `models-v1` is a rolling release: its assets are replaced when an export
# changes, so a pin of the code plus the model digests could no longer fetch
# the bytes it was measured with. This copies every asset of the release into
# a new release tagged `<tag>-<first 12 hex of sha256(SHA256SUMS)>` — the tag
# IS the content identity, so a re-run with unchanged assets is a no-op and a
# replaced asset yields a new tag, never an overwrite of an old one. Consumers
# retrieve frozen bytes with
#   DOCLING_RS_MODELS_URL=https://github.com/<repo>/releases/download/<snapshot-tag>
# and pin them with DOCLING_RS_MODELS_SUMS=<that release's SHA256SUMS>.
# The snapshot is created --latest=false and as a prerelease so install.sh /
# the "latest" release never resolve to it.
set -euo pipefail

TAG="${1:-models-v1}"
REPO="${GITHUB_REPOSITORY:-docling-project/docling.rs}"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

gh release download "$TAG" -R "$REPO" -p SHA256SUMS -D "$WORK"
ID="$(sha256sum "$WORK/SHA256SUMS" | cut -c1-12)"
SNAP="$TAG-$ID"

if gh release view "$SNAP" -R "$REPO" >/dev/null 2>&1; then
  echo "snapshot $SNAP already exists — nothing to do"
  exit 0
fi

gh release download "$TAG" -R "$REPO" -D "$WORK" --clobber
# the downloaded bytes must be the ones SHA256SUMS describes before they are frozen
(cd "$WORK" && grep -v ' SHA256SUMS$' SHA256SUMS | sha256sum -c --quiet)

gh release create "$SNAP" -R "$REPO" --prerelease --latest=false \
  --title "$SNAP (immutable snapshot of $TAG)" \
  --notes "Frozen copy of the \`$TAG\` assets (content id \`$ID\` = sha256 of SHA256SUMS). Never modified after creation; see docs/MODELS.md." \
  "$WORK"/*
echo "created $SNAP"
