#!/usr/bin/env bash
# The workspace crates are released in lockstep, so each one must require
# exactly the version of its siblings it was built and tested with: with a
# caret requirement (`version = "2.85.0"`) the published docling 2.85.0
# resolves docling-core 2.86.14, whose new `Table` field and `NoticeKind`
# variant it does not know, and no longer compiles. Checks that every internal
# path dependency pins the workspace version with `=`, and that the release
# bump (scripts/ci/sync_internal_versions.sh) keeps it that way.
# Offline; usage: scripts/ci/test_internal_pins.sh
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$ROOT"
tmp="$(mktemp -d "${TMPDIR:-/tmp}/internal-pins.XXXXXX")"
trap 'rm -rf "$tmp"' EXIT

# The internal path dependencies of the given manifests that are not exactly
# `=$1`. docling-py is left out: built by maturin from the checkout, it is
# never published to crates.io and its path dependencies carry no version.
unpinned() {
  local want="$1"
  shift
  grep -H 'path = "\.\./docling' "$@" | grep -v 'crates/docling-py/Cargo.toml:' |
    grep -vF "version = \"=$want\"" || true
}

version="$(grep -m1 '^version = ' Cargo.toml | sed -E 's/.*"([^"]+)".*/\1/')"

# 1. the committed manifests
bad="$(unpinned "$version" crates/*/Cargo.toml)"
if [[ -n "$bad" ]]; then
  printf 'FAIL: internal dependency not pinned to =%s:\n%s\n' "$version" "$bad" >&2
  exit 1
fi

# 2. the manifests after a release bump
for manifest in crates/*/Cargo.toml; do
  mkdir -p "$tmp/$(dirname "$manifest")"
  cp "$manifest" "$tmp/$manifest"
done
scripts/ci/sync_internal_versions.sh 9.8.7 "$tmp"/crates/*/Cargo.toml
bad="$(unpinned 9.8.7 "$tmp"/crates/*/Cargo.toml)"
if [[ -n "$bad" ]]; then
  printf 'FAIL: the release bump leaves internal dependencies unpinned:\n%s\n' "$bad" >&2
  exit 1
fi
echo "ok"
