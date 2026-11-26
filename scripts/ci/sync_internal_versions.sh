#!/usr/bin/env bash
#
# Pin every internal path dependency (`path = "../docling…"`) of the given
# manifests to exactly <version>: `version = "=<version>"`.
#
# The crates are released in lockstep from one workspace version, and a 0.x
# "major" bump (scripts/ci/bump_version.sh) is a semver *minor*. A caret
# requirement would let an already-published crate resolve a newer sibling it
# was never built against — docling 2.85.0 picks docling-core 2.86.14 and no
# longer compiles — so each crate requires the exact version released with it.
# Without any requirement the published crates would resolve each other to an
# OLDER release (the version req is a literal, not inherited).
#
# Portable sed (GNU and BSD): no `-i`. Writes the manifests in place.
# Usage: scripts/ci/sync_internal_versions.sh <version> [manifest…]
#        (default manifests: crates/*/Cargo.toml, from the repo root)
set -euo pipefail
new="${1:?usage: sync_internal_versions.sh <version> [manifest…]}"
shift
if [[ $# -eq 0 ]]; then
  cd "$(dirname "$0")/../.."
  set -- crates/*/Cargo.toml
fi
for manifest in "$@"; do
  sed -E "/path = \"\.\.\/docling/ s/version = \"[^\"]+\"/version = \"=$new\"/" "$manifest" >"$manifest.tmp"
  mv "$manifest.tmp" "$manifest"
done
