#!/usr/bin/env bash
#
# Compare Markdown output of Python `docling` vs Rust `fleischwolf` for one
# input file, and show a unified diff.
#
# Usage:
#   scripts/compare.sh <input-file>
#   scripts/compare.sh tests/data/html/sources/example_03.html
#
# Python docling is the latest PUBLISHED release, installed from PyPI into
# fleischwolf/.venv-compare on first run (see _common.sh). For declarative
# formats the Python side calls the format backend directly via
# docling_convert.py, mirroring the work fleischwolf does.

set -euo pipefail

source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/_common.sh"

if [[ $# -lt 1 ]]; then
  echo "usage: $0 <input-file>" >&2
  exit 2
fi

INPUT="$(realpath "$1")"
OUT_DIR="$(mktemp -d)"
trap 'rm -rf "$OUT_DIR"' EXIT

echo ">> input:  $INPUT"
ensure_docling

# --- Python docling --------------------------------------------------------
echo ">> running Python docling ..."
"$PYBIN" "$PY_RUNNER" "$INPUT" "$OUT_DIR/python.md"

# --- Rust fleischwolf -----------------------------------------------------
echo ">> running Rust fleischwolf ..."
cargo run --quiet --manifest-path "$MANIFEST" -p fleischwolf-cli -- "$INPUT" \
  > "$OUT_DIR/rust.md"

# Normalize trailing whitespace/newlines so a single missing final newline
# doesn't show up as a difference on every file.
norm() { printf '%s\n' "$(cat "$1")" > "$1.norm" && mv "$1.norm" "$1"; }
norm "$OUT_DIR/python.md"
norm "$OUT_DIR/rust.md"

# --- Compare ---------------------------------------------------------------
echo ">> python -> $OUT_DIR/python.md"
echo ">> rust   -> $OUT_DIR/rust.md"
echo

if diff -u --label "python/docling" "$OUT_DIR/python.md" \
            --label "rust/fleischwolf" "$OUT_DIR/rust.md"; then
  echo
  echo "✅ IDENTICAL"
else
  echo
  echo "ℹ️  outputs differ (see diff above)"
  # Quick similarity stat: shared lines / python lines.
  shared=$(comm -12 <(sort "$OUT_DIR/python.md") <(sort "$OUT_DIR/rust.md") | grep -c . || true)
  total=$(grep -c . "$OUT_DIR/python.md" || true)
  echo "   shared non-empty lines: ${shared}/${total}"
fi
